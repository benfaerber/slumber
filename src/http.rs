//! HTTP-specific logic and models. [HttpEngine] is the main entrypoint for all
//! operations. This is the life cycle of a request:
//!
//! +--------+
//! | Recipe |
//! +--------+
//!      |
//!  initialize
//!      |
//!      v
//! +-------------+          +-------------------+
//! | RequestSeed | -error-> | RequestBuildError |
//! +-------------+          +-------------------+
//!      |
//!    build
//!      |
//!      v
//! +---------------+
//! | RequestTicket |
//! +---------------+
//!      |
//!    send
//!      |
//!      v
//! +--------+          +--------------+
//! | future | -error-> | RequestError |
//! +--------+          +--------------+
//!      |
//!   success
//!      |
//!      v
//! +----------+
//! | Exchange |
//! +----------+

mod cereal;
mod content_type;
mod models;
mod query;

pub use content_type::*;
pub use models::*;
pub use query::*;

use crate::{
    collection::{Authentication, Method, Recipe},
    config::Config,
    db::CollectionDatabase,
    template::{Template, TemplateContext},
    util::ResultExt,
};
use anyhow::Context;
use bytes::Bytes;
use chrono::Utc;
use futures::future::{self, OptionFuture};
use indexmap::IndexMap;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Response, Url,
};
use std::{collections::HashSet, sync::Arc};
use tokio::try_join;
use tracing::{info, info_span};

const USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Utility for handling all HTTP operations. The main purpose of this is to
/// de-asyncify HTTP so it can be called in the main TUI thread. All heavy
/// lifting will be pushed to background tasks.
///
/// This is safe and cheap to clone because reqwest's `Client` type uses `Arc`
/// internally. [reqwest::Client]
#[derive(Clone, Debug)]
pub struct HttpEngine {
    client: Client,
    /// This client ignores TLS cert errors. Only use it if the user
    /// specifically wants to ignore errors for the request!
    danger_client: Client,
    /// Hostnames for which we should ignore TLS
    danger_hostnames: HashSet<String>,
}

impl HttpEngine {
    /// Build a new HTTP engine, which can be used for the entire program life
    pub fn new(config: &Config) -> Self {
        Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .build()
                .expect("Error building reqwest client"),
            danger_client: Client::builder()
                .user_agent(USER_AGENT)
                .danger_accept_invalid_certs(true)
                .build()
                .expect("Error building reqwest client"),
            danger_hostnames: config
                .ignore_certificate_hosts
                .iter()
                .cloned()
                .collect(),
        }
    }

    /// Build a [RequestTicket] from a [RequestSeed]. This will render the
    /// recipe into a request. The returned ticket can then be launched.
    pub async fn build(
        &self,
        seed: RequestSeed,
        template_context: &TemplateContext,
    ) -> Result<RequestTicket, RequestBuildError> {
        let RequestSeed {
            id,
            recipe,
            options,
        } = &seed;
        let _ =
            info_span!("Build request", request_id = %id, ?recipe, ?options)
                .entered();

        let (client, request) = async {
            // Render everything up front so we can parallelize it
            let (url, query, headers, authentication, body) = try_join!(
                recipe.render_url(template_context),
                recipe.render_query(options, template_context),
                recipe.render_headers(options, template_context),
                recipe.render_authentication(template_context),
                recipe.render_body(template_context),
            )?;

            // Build the reqwest request first, so we can have it do all the
            // hard work of encoding query params/authorization/etc.
            // We'll just copy its homework at the end to get our
            // RequestRecord
            let client = self.get_client(&url);
            let mut builder = client
                .request(recipe.method.into(), url)
                .query(&query)
                .headers(headers);

            match authentication {
                Some(Authentication::Basic { username, password }) => {
                    builder = builder.basic_auth(username, password)
                }
                Some(Authentication::Bearer(token)) => {
                    builder = builder.bearer_auth(token)
                }
                None => {}
            };
            if let Some(body) = body {
                builder = builder.body(body);
            }

            let request = builder.build()?;
            Ok((client, request))
        }
        .await
        .traced()
        .map_err(|error| {
            RequestBuildError::new(
                error,
                &seed,
                template_context.selected_profile.clone(),
            )
        })?;

        Ok(RequestTicket {
            record: RequestRecord::new(
                seed,
                template_context.selected_profile.clone(),
                &request,
            )
            .into(),
            client: client.clone(),
            request,
        })
    }

    /// Render *just* the URL of a request, including query parameters
    pub async fn build_url(
        &self,
        seed: RequestSeed,
        template_context: &TemplateContext,
    ) -> Result<Url, RequestBuildError> {
        let RequestSeed {
            id,
            recipe,
            options,
        } = &seed;
        let _ =
            info_span!("Build request URL", request_id = %id, ?recipe, ?options)
                .entered();

        let request = async {
            // Parallelization!
            let (url, query) = try_join!(
                recipe.render_url(template_context),
                recipe.render_query(options, template_context),
            )?;

            // Use RequestBuilder so we can offload the handling of query params
            let client = self.get_client(&url);
            let request = client
                .request(recipe.method.into(), url)
                .query(&query)
                .build()?;
            Ok(request)
        }
        .await
        .traced()
        .map_err(|error| {
            RequestBuildError::new(
                error,
                &seed,
                template_context.selected_profile.clone(),
            )
        })?;

        Ok(request.url().clone())
    }

    /// Render *just* the body of a request
    pub async fn build_body(
        &self,
        seed: RequestSeed,
        template_context: &TemplateContext,
    ) -> Result<Option<Bytes>, RequestBuildError> {
        let RequestSeed { id, recipe, .. } = &seed;
        let _ = info_span!("Build request body", request_id = %id, ?recipe)
            .entered();

        let body = recipe
            .render_body(template_context)
            .await
            .traced()
            .map_err(|error| {
                RequestBuildError::new(
                    error,
                    &seed,
                    template_context.selected_profile.clone(),
                )
            })?;

        Ok(body)
    }

    /// Get the appropriate client to use for this request. If the request URL's
    /// host is one for which the user wants to ignore TLS certs, use the
    /// dangerous client.
    fn get_client(&self, url: &Url) -> &Client {
        let host = url.host_str().unwrap_or_default();
        if self.danger_hostnames.contains(host) {
            &self.danger_client
        } else {
            &self.client
        }
    }
}

impl RequestTicket {
    /// Launch an HTTP request. Upon completion, it will automatically be
    /// registered in the database for posterity.
    ///
    /// Returns a full HTTP exchange, which includes the originating request,
    /// the response, and the start/end timestamps. We can't report a reliable
    /// start time until after the future is resolved, because the request isn't
    /// launched until the consumer starts awaiting the future. For in-flight
    /// time tracking, track your own start time immediately before/after
    /// sending the request.
    pub async fn send(
        self,
        database: &CollectionDatabase,
    ) -> Result<Exchange, RequestError> {
        let id = self.record.id;

        // Capture the rest of this method in a span
        let _ = info_span!("HTTP request", request_id = %id).entered();

        // This start time will be accurate because the request doesn't launch
        // until this whole future is awaited
        let start_time = Utc::now();
        let result = async {
            let response = self.client.execute(self.request).await?;
            // Load the full response and convert it to our format
            ResponseRecord::from_response(response).await
        }
        .await;
        let end_time = Utc::now();

        match result {
            Ok(response) => {
                info!(status = response.status.as_u16(), "Response");
                let exchange = Exchange {
                    id,
                    request: self.record,
                    response: Arc::new(response),
                    start_time,
                    end_time,
                };

                // Error here should *not* kill the request
                let _ = database.insert_exchange(&exchange);
                Ok(exchange)
            }

            // Attach metadata to the error and yeet it. Can't use map_err
            // because we need to conditionally move the request
            Err(error) => Err(RequestError {
                request: self.record,
                start_time,
                end_time,
                error: error.into(),
            })
            .traced(),
        }
    }
}

impl ResponseRecord {
    /// Convert [reqwest::Response] type into [ResponseRecord]. This is async
    /// because the response content is not necessarily loaded when we first get
    /// the response. Only fails if the response content fails to load.
    async fn from_response(
        response: Response,
    ) -> reqwest::Result<ResponseRecord> {
        // Copy response metadata out first, because we need to move the
        // response to resolve content (not sure why...)
        let status = response.status();
        let headers = response.headers().clone();

        // Pre-resolve the content, so we get all the async work done
        let body = response.bytes().await?.into();

        Ok(ResponseRecord {
            status,
            headers,
            body,
        })
    }
}

/// Render steps for individual pieces of a recipe
impl Recipe {
    /// Render base URL, *excluding* query params
    async fn render_url(
        &self,
        template_context: &TemplateContext,
    ) -> anyhow::Result<Url> {
        let url = self
            .url
            .render_string(template_context)
            .await
            .context("Error rendering URL")?;
        url.parse::<Url>()
            .with_context(|| format!("Invalid URL: `{url}`"))
    }

    /// Render query key=value params
    async fn render_query(
        &self,
        options: &BuildOptions,
        template_context: &TemplateContext,
    ) -> anyhow::Result<IndexMap<String, String>> {
        let iter = self
            .query
            .iter()
            // Filter out disabled params
            .filter(|(param, _)| {
                !options.disabled_query_parameters.contains(*param)
            })
            .map(|(k, v)| async move {
                Ok::<_, anyhow::Error>((
                    k.clone(),
                    v.render_string(template_context).await.context(
                        format!("Error rendering query parameter `{k}`"),
                    )?,
                ))
            });
        Ok(future::try_join_all(iter)
            .await?
            .into_iter()
            .collect::<IndexMap<String, String>>())
    }

    /// Render all headers specified by the user. This will *not* include
    /// authentication and other implicit headers
    async fn render_headers(
        &self,
        options: &BuildOptions,
        template_context: &TemplateContext,
    ) -> anyhow::Result<HeaderMap> {
        let iter = self
            .headers
            .iter()
            // Filter out disabled headers
            .filter(|(header, _)| !options.disabled_headers.contains(*header))
            .map(move |(header, value_template)| {
                self.render_header(template_context, header, value_template)
            });
        let headers = future::try_join_all(iter)
            .await?
            .into_iter()
            .collect::<HeaderMap>();
        Ok(headers)
    }

    /// Render a single key/value header
    async fn render_header(
        &self,
        template_context: &TemplateContext,
        header: &str,
        value_template: &Template,
    ) -> anyhow::Result<(HeaderName, HeaderValue)> {
        let mut value = value_template
            .render(template_context)
            .await
            .context(format!("Error rendering header `{header}`"))?;

        // Strip leading/trailing line breaks because they're going to trigger a
        // validation error and are probably a mistake. We're trading
        // explicitness for convenience here. This is maybe redundant now with
        // the Chain::trim field, but this behavior predates that field so it's
        // left in for backward compatibility.
        trim_bytes(&mut value, |c| c == b'\n' || c == b'\r');

        // String -> header conversions are fallible, if headers
        // are invalid
        Ok::<(HeaderName, HeaderValue), anyhow::Error>((
            header
                .try_into()
                .context(format!("Error encoding header name `{header}`"))?,
            value.try_into().context(format!(
                "Error encoding value for header `{header}`"
            ))?,
        ))
    }

    /// Render authentication and return the same data structure, with resolved
    /// data. This can be passed to [reqwest::RequestBuilder]
    async fn render_authentication(
        &self,
        template_context: &TemplateContext,
    ) -> anyhow::Result<Option<Authentication<String>>> {
        match &self.authentication {
            Some(Authentication::Basic { username, password }) => {
                let (username, password) = try_join!(
                    async {
                        username
                            .render_string(template_context)
                            .await
                            .context("Error rendering username")
                    },
                    async {
                        OptionFuture::from(password.as_ref().map(|password| {
                            password.render_string(template_context)
                        }))
                        .await
                        .transpose()
                        .context("Error rendering password")
                    },
                )?;
                Ok(Some(Authentication::Basic { username, password }))
            }

            Some(Authentication::Bearer(token)) => {
                let token = token
                    .render_string(template_context)
                    .await
                    .context("Error rendering bearer token")?;
                Ok(Some(Authentication::Bearer(token)))
            }
            None => Ok(None),
        }
    }

    /// Render request body
    async fn render_body(
        &self,
        template_context: &TemplateContext,
    ) -> anyhow::Result<Option<Bytes>> {
        if let Some(body) = &self.body {
            let rendered = body
                .render(template_context)
                .await
                .context("Error rendering body")?;
            Ok(Some(rendered.into()))
        } else {
            Ok(None)
        }
    }
}

impl From<Method> for reqwest::Method {
    fn from(method: Method) -> Self {
        match method {
            Method::Connect => reqwest::Method::CONNECT,
            Method::Delete => reqwest::Method::DELETE,
            Method::Get => reqwest::Method::GET,
            Method::Head => reqwest::Method::HEAD,
            Method::Options => reqwest::Method::OPTIONS,
            Method::Patch => reqwest::Method::PATCH,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Trace => reqwest::Method::TRACE,
        }
    }
}

/// Trim the bytes from the beginning and end of a vector that match the given
/// predicate. This will mutate the input vector. If bytes are trimmed off the
/// start, it will be done with a single shift.
fn trim_bytes(bytes: &mut Vec<u8>, f: impl Fn(u8) -> bool) {
    // Trim start
    for i in 0..bytes.len() {
        if !f(bytes[i]) {
            bytes.drain(0..i);
            break;
        }
    }

    // Trim end
    for i in (0..bytes.len()).rev() {
        if !f(bytes[i]) {
            bytes.drain((i + 1)..bytes.len());
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        collection::{self, Authentication, Collection, Profile},
        test_util::{header_map, Factory},
    };
    use indexmap::indexmap;
    use pretty_assertions::assert_eq;
    use reqwest::{Method, StatusCode};
    use rstest::{fixture, rstest};
    use std::collections::HashMap;

    #[fixture]
    fn http_engine() -> HttpEngine {
        HttpEngine::new(&Config::default())
    }

    #[fixture]
    fn template_context() -> TemplateContext {
        let profile_data = indexmap! {
            "host".into() => "http://localhost".into(),
            "mode".into() => "sudo".into(),
            "user_id".into() => "1".into(),
            "group_id".into() => "3".into(),
            "token".into() => "hunter2".into(),
        };
        let profile = Profile {
            data: profile_data,
            ..Profile::factory(())
        };
        let profile_id = profile.id.clone();
        TemplateContext {
            collection: Collection {
                profiles: indexmap! {profile_id.clone() => profile},
                ..Collection::factory(())
            },
            selected_profile: Some(profile_id.clone()),
            ..TemplateContext::factory(())
        }
    }

    #[rstest]
    #[tokio::test]
    async fn test_build_request(
        http_engine: HttpEngine,
        template_context: TemplateContext,
    ) {
        let recipe = Recipe {
            method: collection::Method::Post,
            url: "{{host}}/users/{{user_id}}".into(),
            query: indexmap! {
                "mode".into() => "{{mode}}".into(),
                "fast".into() => "true".into(),
            },
            headers: indexmap! {
                // Leading/trailing newlines should be stripped
                "Accept".into() => "application/json".into(),
                "Content-Type".into() => "application/json".into(),
            },
            body: Some("{\"group_id\":\"{{group_id}}\"}".into()),
            ..Recipe::factory(())
        };
        let recipe_id = recipe.id.clone();

        let seed = RequestSeed::new(recipe, BuildOptions::default());
        let ticket = http_engine.build(seed, &template_context).await.unwrap();

        let expected_headers = indexmap! {
            "content-type" => "application/json",
            "accept" => "application/json",
        };

        assert_eq!(
            *ticket.record,
            RequestRecord {
                id: ticket.record.id,
                profile_id: Some(
                    template_context.collection.first_profile_id().clone()
                ),
                recipe_id,
                method: Method::POST,
                url: "http://localhost/users/1?mode=sudo&fast=true"
                    .parse()
                    .unwrap(),
                body: Some(Vec::from(b"{\"group_id\":\"3\"}").into()),
                headers: header_map(expected_headers),
            }
        );
    }

    /// Test building just a URL. Should include query params, but headers/body
    /// should *not* be built
    #[rstest]
    #[tokio::test]
    async fn test_build_url(
        http_engine: HttpEngine,
        template_context: TemplateContext,
    ) {
        let recipe = Recipe {
            url: "{{host}}/users/{{user_id}}".into(),
            query: indexmap! {
                "mode".into() => "{{mode}}".into(),
                "fast".into() => "true".into(),
            },
            ..Recipe::factory(())
        };

        let seed = RequestSeed::new(recipe, BuildOptions::default());
        let url = http_engine
            .build_url(seed, &template_context)
            .await
            .unwrap();

        assert_eq!(
            url.as_str(),
            "http://localhost/users/1?mode=sudo&fast=true"
        );
    }

    /// Test building just a body. URL/query/headers should *not* be built.
    #[rstest]
    #[tokio::test]
    async fn test_build_body(
        http_engine: HttpEngine,
        template_context: TemplateContext,
    ) {
        let recipe = Recipe {
            body: Some(r#"{"group_id":"{{group_id}}"}"#.into()),
            ..Recipe::factory(())
        };

        let seed = RequestSeed::new(recipe, BuildOptions::default());
        let body = http_engine
            .build_body(seed, &template_context)
            .await
            .unwrap();

        assert_eq!(body.as_deref(), Some(br#"{"group_id":"3"}"#.as_slice()));
    }

    /// Test launching a built request
    #[rstest]
    #[tokio::test]
    async fn test_send_request(
        http_engine: HttpEngine,
        template_context: TemplateContext,
    ) {
        // Mock HTTP response
        let mut server = mockito::Server::new_async().await;
        let url = server.url();
        let mock = server
            .mock("GET", "/get")
            .with_status(200)
            .with_body("hello!")
            .create_async()
            .await;

        let recipe = Recipe {
            url: format!("{url}/get").as_str().into(),
            ..Recipe::factory(())
        };

        // Build+send the request
        let seed = RequestSeed::new(recipe, BuildOptions::default());
        let ticket = http_engine.build(seed, &template_context).await.unwrap();
        let exchange = ticket.send(&template_context.database).await.unwrap();

        // Cheat on this one, because we don't know exactly when the server
        // resolved it
        let date_header = exchange
            .response
            .headers
            .get("date")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            *exchange.response,
            ResponseRecord {
                status: StatusCode::OK,
                headers: header_map([
                    ("connection", "close"),
                    ("content-length", "6"),
                    ("date", date_header),
                ]),
                body: ResponseBody::new(b"hello!".as_slice().into())
            }
        );

        mock.assert();
    }

    /// Test building requests with various authentication methods
    #[rstest]
    #[case::basic(
        Authentication::Basic {
            username: "{{username}}".into(),
            password: Some("{{password}}".into()),
        },
        "Basic dXNlcjpodW50ZXIy"
    )]
    #[case::basic_no_password(
        Authentication::Basic {
            username: "{{username}}".into(),
            password: None,
        },
        "Basic dXNlcjo="
    )]
    #[case::bearer(Authentication::Bearer("{{token}}".into()), "Bearer token!")]
    #[tokio::test]
    async fn test_authentication(
        http_engine: HttpEngine,
        #[case] authentication: Authentication,
        #[case] expected_header: &str,
    ) {
        let profile_data = indexmap! {
            "username".into() => "user".into(),
            "password".into() => "hunter2".into(),
            "token".into() => "token!".into(),
        };
        let profile = Profile {
            data: profile_data,
            ..Profile::factory(())
        };
        let profile_id = profile.id.clone();
        let template_context = TemplateContext {
            collection: Collection {
                profiles: indexmap! {profile_id.clone() => profile},
                ..Collection::factory(())
            },
            selected_profile: Some(profile_id.clone()),
            ..TemplateContext::factory(())
        };
        let recipe = Recipe {
            authentication: Some(authentication),
            ..Recipe::factory(())
        };
        let recipe_id = recipe.id.clone();

        let seed = RequestSeed::new(recipe, BuildOptions::default());
        let ticket = http_engine.build(seed, &template_context).await.unwrap();

        let expected_headers: HashMap<String, String> =
            [("authorization", expected_header)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

        assert_eq!(
            *ticket.record,
            RequestRecord {
                id: ticket.record.id,
                profile_id: Some(profile_id),
                recipe_id,
                method: Method::GET,
                url: "http://localhost/url".parse().unwrap(),
                headers: (&expected_headers).try_into().unwrap(),
                body: None,
            }
        );
    }

    #[rstest]
    #[tokio::test]
    async fn test_disable_headers_and_query_params(
        http_engine: HttpEngine,
        template_context: TemplateContext,
    ) {
        let recipe = Recipe {
            query: indexmap! {
                "mode".into() => "sudo".into(),
                "fast".into() => "true".into(),
            },
            headers: indexmap! {
                "Accept".into() => "application/json".into(),
                "Content-Type".into() => "application/json".into(),
            },
            ..Recipe::factory(())
        };
        let recipe_id = recipe.id.clone();

        let seed = RequestSeed::new(
            recipe,
            BuildOptions {
                disabled_headers: ["Content-Type".to_owned()].into(),
                disabled_query_parameters: ["fast".to_owned()].into(),
            },
        );
        let ticket = http_engine.build(seed, &template_context).await.unwrap();

        let expected_headers: HashMap<String, String> =
            [("accept", "application/json")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

        assert_eq!(
            *ticket.record,
            RequestRecord {
                id: ticket.record.id,
                profile_id: template_context.selected_profile.clone(),
                recipe_id,
                method: Method::GET,
                url: "http://localhost/url?mode=sudo".parse().unwrap(),
                headers: (&expected_headers).try_into().unwrap(),
                body: None,
            }
        );
    }

    /// Leading/trailing newlines should be stripped from rendered header
    /// values. These characters are invalid and trigger an error, so we assume
    /// they're unintentional and the user won't miss them.
    #[rstest]
    #[tokio::test]
    async fn test_render_headers_strip(template_context: TemplateContext) {
        let recipe = Recipe {
            // Leading/trailing newlines should be stripped
            headers: indexmap! {
                "Accept".into() => "application/json".into(),
                "Host".into() => "\n{{host}}\n".into(),
            },
            ..Recipe::factory(())
        };
        let rendered = recipe
            .render_headers(&BuildOptions::default(), &template_context)
            .await
            .unwrap();

        assert_eq!(
            rendered,
            header_map([
                ("Accept", "application/json"),
                // This is a non-sensical value, but it's good enough
                ("Host", "http://localhost"),
            ])
        );
    }

    #[rstest]
    #[case::empty(&[], &[])]
    #[case::start(&[0, 0, 1, 1], &[1, 1])]
    #[case::end(&[1, 1, 0, 0], &[1, 1])]
    #[case::both(&[0, 1, 0, 1, 0, 0], &[1, 0, 1])]
    fn test_trim_bytes(#[case] bytes: &[u8], #[case] expected: &[u8]) {
        let mut bytes = bytes.to_owned();
        trim_bytes(&mut bytes, |b| b == 0);
        assert_eq!(&bytes, expected);
    }
}
