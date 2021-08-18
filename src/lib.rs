//! CSRF attack mitigation.
//!
//! This middleware is mitigating the CSRF attacks by using the double token submit
//! A token is sent to the client via the set-cookie header. Then, the client will
//! send the token in its request by two different ways:
//! - First, in the cookie header.
//! - Then in its request body/header/parameters depending on the middleware configuration.
//!
//! For methods that are checked (POST, PUT, DELETE...), any issue will return an error
//! 400.
//!
//! Basic usage is:
//! ```
//!
//! use actix_csrf::Csrf;
//! use actix_web::{HttpServer, web, App, HttpResponse};
//!
//! let server = HttpServer::new(|| {
//!     App::new()
//!         .wrap(Csrf::new())
//!         .service(web::resource("/")
//!             // by default will not check get
//!             .route(web::get().to(|| HttpResponse::Ok()))
//!             // by default will check post
//!             .route(web::post().to(|| HttpResponse::Ok())))
//! });
//!
//! ```
//!
//! You can deactivate the protection (for example for dev mode or unit tests.
//! ```
//! use actix_csrf::Csrf;
//! Csrf::new().set_enabled(false);
//! ```
//!

use actix_web::cookie::Cookie;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::header::{self, HeaderValue};
use actix_web::http::{Method, StatusCode};
use actix_web::{Either, HttpMessage, HttpResponse, ResponseError};
use log::error;
use rand::prelude::StdRng;
use rand::{CryptoRng, SeedableRng};
use std::collections::HashMap;
use std::convert::Infallible;
use std::default::Default;
use std::fmt::Display;
use std::future::{self, Future, Ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

pub mod extractor;
pub mod generator;

/// Internal errors that can happen when processing CSRF tokens.
#[derive(Debug)]
pub enum CsrfError {
    /// The CSRF Token and the token provided in the headers do not match
    TokenDontMatch,
    /// No CSRF Token in the cookies.
    MissingCookie,
    /// No CSRF Token in the request (headers/body...).
    MissingToken(String),
}

impl Display for CsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CsrfError::TokenDontMatch => write!(f, "The CSRF Tokens do not match"),
            CsrfError::MissingCookie => write!(f, "The CSRF Token is missing in the cookies"),
            CsrfError::MissingToken(token) => write!(f, "The CSRF Token is missing = {}", token),
        }
    }
}

impl ResponseError for CsrfError {
    fn error_response(&self) -> HttpResponse {
        // I don't really want to leak the error to the client. But I need
        // to log it as CSRF attacks are a thing.
        error!("{}", self);
        HttpResponse::with_body(StatusCode::BAD_REQUEST, format!("CSRF Error").into())
    }
}

/// Middleware builder. The default will check CSRF on every request but
/// GET and POST. You can specify whether to disable.
pub struct Csrf<Rng> {
    inner: Inner<Rng>,
}

impl Csrf<StdRng> {
    /// Create the CSRF default middleware
    pub fn new() -> Self {
        Self {
            inner: Inner::default(),
        }
    }
}

impl<Rng> Csrf<Rng> {
    /// Control whether we check for the token on requests.
    pub fn set_enabled(mut self, enabled: bool) -> Self {
        self.inner.csrf_enabled = enabled;
        self
    }

    /// Add an extractor for the specified method.
    pub fn add_extractor(mut self, method: Method, extractor: Box<extractor::Extractor>) -> Self {
        self.inner.req_extractors.insert(method, extractor);
        self
    }

    /// Replace all the extractors
    pub fn set_extractors(
        mut self,
        extractors: HashMap<Method, Box<extractor::Extractor>>,
    ) -> Self {
        self.inner.req_extractors = extractors;
        self
    }

    /// Add a whitelisted endpoint
    pub fn add_whilelist(mut self, method: Method, uri: String) -> Self {
        self.inner.whitelist.push((method, uri));
        self
    }
}

impl<S, Rng> Transform<S> for Csrf<Rng>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse>,
    Rng: Clone,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse;
    type Error = S::Error;
    type InitError = ();
    type Transform = CsrfMiddleware<S, Rng>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ready(Ok(CsrfMiddleware {
            service,
            inner: self.inner.clone(),
        }))
    }
}

pub struct CsrfMiddleware<S, Rng> {
    service: S,
    inner: Inner<Rng>,
}

#[derive(Clone)]
struct Inner<Rng> {
    /// To generate the token
    generator: Rng,

    cookie_name: String,

    /// If false, will not check at all for CSRF tokens
    csrf_enabled: bool,

    /// Extract the token from an incoming HTTP request. One extractor
    /// per Method.
    req_extractors: HashMap<Method, Box<extractor::Extractor>>,

    /// Endpoints that are not protected by the middleware.
    /// Mapping of Method to URI.
    whitelist: Vec<(Method, String)>,
}

impl Default for Inner<StdRng> {
    fn default() -> Self {
        // sane defaults?
        let generator = StdRng::from_entropy();
        let mut req_extractors: HashMap<Method, Box<extractor::Extractor>> = HashMap::new();
        let cookie_name = String::from("csrfToken");
        req_extractors.insert(
            Method::POST,
            Box::new(extractor::BasicExtractor::Header {
                name: "x-csrf-token".to_owned(),
            }),
        );

        req_extractors.insert(
            Method::PUT,
            Box::new(extractor::BasicExtractor::Header {
                name: "x-csrf-token".to_owned(),
            }),
        );

        req_extractors.insert(
            Method::DELETE,
            Box::new(extractor::BasicExtractor::Header {
                name: "x-csrf-token".to_owned(),
            }),
        );

        Self {
            generator,
            cookie_name,
            req_extractors,
            whitelist: vec![],
            csrf_enabled: true,
        }
    }
}

impl<Rng> Inner<Rng> {
    /// Will return true if the middleware needs to check the CSRF tokens.
    fn should_protect(&self, req: &ServiceRequest) -> bool {
        if self.in_whilelist(&req.method(), req.path()) {
            return false;
        }

        (self.req_extractors.contains_key(req.method())) && self.csrf_enabled
    }

    fn in_whilelist(&self, req_method: &Method, req_uri: &str) -> bool {
        for (method, uri) in &self.whitelist {
            if method == req_method && uri == req_uri {
                return true;
            }
        }

        false
    }

    /// Will extract the token from a cookie that was set previously.
    fn extract_cookie_token(&self, req: &ServiceRequest) -> Result<String, CsrfError> {
        req.cookie(&self.cookie_name)
            .map(|cookie| cookie.value().to_string())
            .ok_or(CsrfError::MissingCookie)
    }

    /// Will extract the matching token from the request.
    fn extract_request_token(&self, req: &ServiceRequest) -> Result<String, CsrfError> {
        // Unwrap. At this point, if we arrive here, there is no doubt we have
        // an extractor or it means there is a coding error.
        self.req_extractors
            .get(&req.method())
            .unwrap()
            .extract_token(&req)
    }
}

impl<Rng: CryptoRng> Inner<Rng> {
    /// Generate the next token
    fn generate_token(&mut self) -> String {
        todo!();
        // self.generator.generate_token()
    }
}

impl<S, Rng> Service for CsrfMiddleware<S, Rng>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse>,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse;
    type Error = S::Error;
    type Future = CsrfMiddlewareFuture<S>;

    fn call(&mut self, req: ServiceRequest) -> Self::Future {
        // Before request, we need to check that for protected resources, the CSRF
        // tokens are actually there and matching. By default protected resources
        // are everything but GET and OPTIONS but you might want to also protect
        // GET if it has server side effects.
        if self.inner.should_protect(&req) {
            // First make sure the tokens are both here
            let cookie_token = self.inner.extract_cookie_token(&req);
            let req_token = self.inner.extract_request_token(&req);

            match (cookie_token, req_token) {
                (Err(e), _) | (_, Err(e)) => {
                    return CsrfMiddlewareFuture::CsrfError(req.error_response(e));
                }
                (Ok(ref cookie_token), Ok(ref req_token)) if cookie_token != req_token => {
                    println!("COOKIE {:?} HEADER {:?}", cookie_token, req_token);
                    return CsrfMiddlewareFuture::CsrfError(
                        req.error_response(CsrfError::TokenDontMatch),
                    );
                }
                _ => (), // tokens match, continue
            }
        }

        // TODO Lifetime issue when I put that in and_then
        // let token = self.inner.generate_token();
        // let cookie_name = self.inner.cookie_name.clone();
        // let enabled = self.inner.csrf_enabled.clone();

        let fut = self.service.call(req);

        // let fut = async {
        //     self.service.call(req).await.and_then(move |mut res| {
        //         // Set the newly generated token.
        //         let mut cookie = Cookie::new(cookie_name, token);
        //         cookie.set_path("/");

        //         if enabled {
        //             res.response_mut().headers_mut().insert(
        //                 header::SET_COOKIE,
        //                 HeaderValue::from_str(&cookie.to_string()).unwrap(),
        //             );
        //         }

        //         Ok(res)
        //     })
        // };

        // Box::pin(fut)

        CsrfMiddlewareFuture::Passthrough(Box::pin(fut))
    }

    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx)
    }
}

pub enum CsrfMiddlewareFuture<S: Service<Request = ServiceRequest>> {
    CsrfError(ServiceResponse),
    Passthrough(Pin<Box<S::Future>>),
}

impl<S> Future for CsrfMiddlewareFuture<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse>,
{
    type Output = Result<ServiceResponse, S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            CsrfMiddlewareFuture::CsrfError(error) => todo!(),
            CsrfMiddlewareFuture::Passthrough(service) => Pin::new(service).poll(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use actix_web::http::StatusCode;
    use actix_web::test::{self, TestRequest};
    use actix_web::{web, App, HttpResponse};

    fn get_token_from_resp(resp: &ServiceResponse) -> String {
        // Cookie should be in the response.
        let cookie_header: Vec<_> = resp
            .headers()
            .iter()
            .filter(|(header_name, _)| header_name.as_str() == "set-cookie")
            .map(|(_, value)| String::from(value.to_str().unwrap()))
            .collect();
        assert_eq!(1, cookie_header.len());
        assert!(cookie_header.get(0).unwrap().contains("csrfToken"));

        // should be something like "csrfToken=NHMWzEq7nAFZR56jnanhFv6WJdeEAyhy; Path=/"
        println!("{:?}", cookie_header.get(0).unwrap());
        let token_header: String = cookie_header.get(0).take().unwrap().to_string();
        let token = &token_header[10..42];
        String::from(token)
    }

    fn get_cookie_from_resp(resp: &ServiceResponse) -> String {
        let cookie_header: Vec<_> = resp
            .headers()
            .iter()
            .filter(|(header_name, _)| header_name.as_str() == "set-cookie")
            .map(|(_, value)| String::from(value.to_str().unwrap()))
            .collect();
        assert_eq!(1, cookie_header.len());
        String::from(cookie_header.get(0).unwrap().as_str())
    }

    // Check that the CSRF token is correctly attached to the response
    #[tokio::test]
    async fn test_attach_token() {
        let mut srv = test::init_service(
            App::new()
                .wrap(Csrf::new())
                .service(web::resource("/").to(|| HttpResponse::Ok())),
        )
        .await;
        let resp = test::call_service(&mut srv, TestRequest::with_uri("/").to_request()).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Cookie should be in the response.
        let cookie_header: Vec<_> = resp
            .headers()
            .iter()
            .filter(|(header_name, _)| header_name.as_str() == "set-cookie")
            .map(|(_, value)| String::from(value.to_str().unwrap()))
            .collect();
        assert_eq!(1, cookie_header.len());
        assert!(cookie_header.get(0).unwrap().contains("csrfToken"));
    }

    // With default protection, POST requests is rejected.
    #[tokio::test]
    async fn test_post_request_rejected() {
        let mut srv = test::init_service(
            App::new()
                .wrap(Csrf::new())
                .service(web::resource("/").route(web::post().to(|| HttpResponse::Ok()))),
        )
        .await;
        let resp = test::call_service(&mut srv, TestRequest::post().uri("/").to_request()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // Can disable protection for unit tests.
    #[tokio::test]
    async fn test_post_accepted_with_disabled() {
        let mut srv = test::init_service(
            App::new()
                .wrap(Csrf::new().set_enabled(false))
                .service(web::resource("/").route(web::post().to(|| HttpResponse::Ok()))),
        )
        .await;
        let resp = test::call_service(&mut srv, TestRequest::post().uri("/").to_request()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie_header: Vec<_> = resp
            .headers()
            .iter()
            .filter(|(header_name, _)| header_name.as_str() == "set-cookie")
            .map(|(_, value)| String::from(value.to_str().unwrap()))
            .collect();

        assert_eq!(0, cookie_header.len());
    }

    /// Will use double submit method.
    #[tokio::test]
    async fn double_submit_correct_token() {
        let mut srv = test::init_service(
            App::new().wrap(Csrf::new()).service(
                web::resource("/")
                    .route(web::get().to(|| HttpResponse::Ok()))
                    .route(web::post().to(|| HttpResponse::Ok())),
            ),
        )
        .await;

        // First, let's get the token as a client.
        let resp = test::call_service(&mut srv, TestRequest::with_uri("/").to_request()).await;

        let token = get_token_from_resp(&resp);
        let cookie = get_cookie_from_resp(&resp);

        // Now we can do another request to a protected endpoint.
        let req = TestRequest::post()
            .uri("/")
            .header("cookie", cookie)
            .header("x-csrf-token", token)
            .to_request();
        let resp = test::call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_whitelist() {
        let mut srv = test::init_service(
            App::new()
                .wrap(Csrf::new().add_whilelist(Method::POST, "/".to_string()))
                .service(web::resource("/").route(web::post().to(|| HttpResponse::Ok()))),
        )
        .await;
        let resp = test::call_service(&mut srv, TestRequest::post().uri("/").to_request()).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Cookie should be in the response.
        let cookie_header: Vec<_> = resp
            .headers()
            .iter()
            .filter(|(header_name, _)| header_name.as_str() == "set-cookie")
            .map(|(_, value)| String::from(value.to_str().unwrap()))
            .collect();
        assert_eq!(1, cookie_header.len());
        assert!(cookie_header.get(0).unwrap().contains("csrfToken"));
    }
}
