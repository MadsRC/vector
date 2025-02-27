use std::collections::HashMap;

use aws_types::credentials::SharedCredentialsProvider;
use aws_types::region::Region;
use bytes::{Buf, Bytes};
use http::{Response, StatusCode, Uri};
use hyper::{body, Body};
use serde::Deserialize;
use snafu::ResultExt;
use vector_core::config::proxy::ProxyConfig;
use vector_core::config::LogNamespace;

use super::{
    request_builder::ElasticsearchRequestBuilder, ElasticsearchApiVersion, ElasticsearchEncoder,
    InvalidHostSnafu, Request,
};
use crate::{
    http::{Auth, HttpClient, MaybeAuth},
    sinks::{
        elasticsearch::{
            ElasticsearchAuth, ElasticsearchCommonMode, ElasticsearchConfig, ParseError,
        },
        util::{http::RequestConfig, TowerRequestConfig, UriSerde},
        HealthcheckError,
    },
    tls::TlsSettings,
    transforms::metric_to_log::MetricToLog,
};

#[derive(Debug, Clone)]
pub struct ElasticsearchCommon {
    pub base_url: String,
    pub bulk_uri: Uri,
    pub http_auth: Option<Auth>,
    pub aws_auth: Option<SharedCredentialsProvider>,
    pub mode: ElasticsearchCommonMode,
    pub request_builder: ElasticsearchRequestBuilder,
    pub tls_settings: TlsSettings,
    pub region: Option<Region>,
    pub request: RequestConfig,
    pub query_params: HashMap<String, String>,
    pub metric_to_log: MetricToLog,
}

impl ElasticsearchCommon {
    pub async fn parse_config(
        config: &ElasticsearchConfig,
        endpoint: &str,
        proxy_config: &ProxyConfig,
        version: &mut Option<usize>,
    ) -> crate::Result<Self> {
        // Test the configured host, but ignore the result
        let uri = format!("{}/_test", endpoint);
        let uri = uri
            .parse::<Uri>()
            .with_context(|_| InvalidHostSnafu { host: endpoint })?;
        if uri.host().is_none() {
            return Err(ParseError::HostMustIncludeHostname {
                host: endpoint.to_string(),
            }
            .into());
        }

        let authorization = match &config.auth {
            Some(ElasticsearchAuth::Basic { user, password }) => Some(Auth::Basic {
                user: user.clone(),
                password: password.clone(),
            }),
            _ => None,
        };
        let uri = endpoint.parse::<UriSerde>()?;
        let http_auth = authorization.choose_one(&uri.auth)?;
        let base_url = uri.uri.to_string().trim_end_matches('/').to_owned();

        let aws_auth = match &config.auth {
            Some(ElasticsearchAuth::Basic { .. }) | None => None,
            Some(ElasticsearchAuth::Aws(aws)) => {
                let region = config
                    .aws
                    .as_ref()
                    .map(|config| config.region())
                    .ok_or(ParseError::RegionRequired)?
                    .ok_or(ParseError::RegionRequired)?;

                Some(aws.credentials_provider(region).await?)
            }
        };

        let mode = config.common_mode()?;

        let tower_request = config
            .request
            .tower
            .unwrap_with(&TowerRequestConfig::default());

        let mut query_params = config.query.clone().unwrap_or_default();
        query_params.insert(
            "timeout".into(),
            format!("{}s", tower_request.timeout.as_secs()),
        );

        if let Some(pipeline) = &config.pipeline {
            query_params.insert("pipeline".into(), pipeline.into());
        }

        let bulk_url = {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            for (p, v) in &query_params {
                query.append_pair(&p[..], &v[..]);
            }
            format!("{}/_bulk?{}", base_url, query.finish())
        };
        let bulk_uri = bulk_url.parse::<Uri>().unwrap();

        let tls_settings = TlsSettings::from_options(&config.tls)?;
        let config = config.clone();
        let request = config.request;

        let metric_config = config.metrics.clone().unwrap_or_default();
        let metric_to_log = MetricToLog::new(
            metric_config.host_tag.as_deref(),
            metric_config.timezone.unwrap_or_default(),
            LogNamespace::Legacy,
            metric_config.metric_tag_values,
        );

        let region = config.aws.as_ref().and_then(|config| config.region());

        let version = if let Some(version) = *version {
            version
        } else {
            let ver = match config.api_version {
                ElasticsearchApiVersion::V6 => 6,
                ElasticsearchApiVersion::V7 => 7,
                ElasticsearchApiVersion::V8 => 8,
                ElasticsearchApiVersion::Auto => {
                    match get_version(
                        &base_url,
                        &http_auth,
                        &aws_auth,
                        &region,
                        &request,
                        &tls_settings,
                        proxy_config,
                    )
                    .await
                    {
                        Ok(version) => version,
                        // This error should be fatal, but for now we only emit it as a warning
                        // to make the transition smoother.
                        Err(error) => {
                            // For now, estimate version.
                            // The `suppress_type_name` option is only valid up to V6, so if a user
                            // specified that is true, then we will assume they need API V6.
                            // Otherwise, assume the latest version (V8).
                            // This is by no means a perfect assumption but it's the best we can
                            // make with the data we have.
                            let assumed_version = if config.suppress_type_name { 6 } else { 8 };
                            debug!(message = "Assumed ElasticsearchApi based on config setting suppress_type_name.",
                                   %assumed_version,
                                   %config.suppress_type_name
                            );
                            warn!(message = "Failed to determine Elasticsearch version from `/_cluster/state/version`. Please fix the reported error or set an API version explicitly via `api_version`.",
                                  %assumed_version,
                                  %error
                            );
                            assumed_version
                        }
                    }
                }
            };
            *version = Some(ver);
            ver
        };

        let doc_type = config.doc_type.clone();
        let suppress_type_name = if config.suppress_type_name {
            warn!(message = "DEPRECATION, use of deprecated option `suppress_type_name`. Please use `api_version` option instead.");
            config.suppress_type_name
        } else {
            version >= 7
        };
        let request_builder = ElasticsearchRequestBuilder {
            compression: config.compression,
            encoder: ElasticsearchEncoder {
                transformer: config.encoding.clone(),
                doc_type,
                suppress_type_name,
            },
        };

        Ok(Self {
            http_auth,
            base_url,
            bulk_uri,
            aws_auth,
            mode,
            request_builder,
            query_params,
            request,
            region,
            tls_settings,
            metric_to_log,
        })
    }

    /// Parses endpoints into a vector of ElasticsearchCommons. The resulting vector is guaranteed to not be empty.
    pub async fn parse_many(
        config: &ElasticsearchConfig,
        proxy_config: &ProxyConfig,
    ) -> crate::Result<Vec<Self>> {
        let mut version = None;
        if let Some(endpoint) = config.endpoint.as_ref() {
            warn!(message = "DEPRECATION, use of deprecated option `endpoint`. Please use `endpoints` option instead.");
            if config.endpoints.is_empty() {
                Ok(vec![
                    Self::parse_config(config, endpoint, proxy_config, &mut version).await?,
                ])
            } else {
                Err(ParseError::EndpointsExclusive.into())
            }
        } else if config.endpoints.is_empty() {
            Err(ParseError::EndpointRequired.into())
        } else {
            let mut commons = Vec::new();
            for endpoint in config.endpoints.iter() {
                commons
                    .push(Self::parse_config(config, endpoint, proxy_config, &mut version).await?);
            }
            Ok(commons)
        }
    }

    /// Parses a single endpoint, else panics.
    #[cfg(test)]
    pub async fn parse_single(config: &ElasticsearchConfig) -> crate::Result<Self> {
        let mut commons =
            Self::parse_many(config, crate::config::SinkContext::new_test().proxy()).await?;
        assert_eq!(commons.len(), 1);
        Ok(commons.remove(0))
    }

    pub async fn healthcheck(self, client: HttpClient) -> crate::Result<()> {
        match get(
            &self.base_url,
            &self.http_auth,
            &self.aws_auth,
            &self.region,
            &self.request,
            client,
            "/_cluster/health",
        )
        .await?
        .status()
        {
            StatusCode::OK => Ok(()),
            status => Err(HealthcheckError::UnexpectedStatus { status }.into()),
        }
    }
}

pub async fn sign_request(
    request: &mut http::Request<Bytes>,
    credentials_provider: &SharedCredentialsProvider,
    region: &Option<Region>,
) -> crate::Result<()> {
    crate::aws::sign_request("es", request, credentials_provider, region).await
}

async fn get_version(
    base_url: &str,
    http_auth: &Option<Auth>,
    aws_auth: &Option<SharedCredentialsProvider>,
    region: &Option<Region>,
    request: &RequestConfig,
    tls_settings: &TlsSettings,
    proxy_config: &ProxyConfig,
) -> crate::Result<usize> {
    #[derive(Deserialize)]
    struct ClusterState {
        version: Option<usize>,
    }

    let client = HttpClient::new(tls_settings.clone(), proxy_config)?;
    let response = get(
        base_url,
        http_auth,
        aws_auth,
        region,
        request,
        client,
        "/_cluster/state/version",
    )
    .await
    .map_err(|error| format!("Failed to get Elasticsearch API version: {}", error))?;

    let (_, body) = response.into_parts();
    let mut body = body::aggregate(body).await?;
    let body = body.copy_to_bytes(body.remaining());
    let ClusterState { version } = serde_json::from_slice(&body)?;
    version.ok_or_else(||"Unexpected response from Elasticsearch endpoint `/_cluster/state/version`. Missing `version`. Consider setting `api_version` option.".into())
}

async fn get(
    base_url: &str,
    http_auth: &Option<Auth>,
    aws_auth: &Option<SharedCredentialsProvider>,
    region: &Option<Region>,
    request: &RequestConfig,
    client: HttpClient,
    path: &str,
) -> crate::Result<Response<Body>> {
    let mut builder = Request::get(format!("{}{}", base_url, path));

    if let Some(authorization) = &http_auth {
        builder = authorization.apply_builder(builder);
    }

    for (header, value) in &request.headers {
        builder = builder.header(&header[..], &value[..]);
    }

    let mut request = builder.body(Bytes::new())?;

    if let Some(credentials_provider) = aws_auth {
        sign_request(&mut request, credentials_provider, region).await?;
    }
    client
        .send(request.map(hyper::Body::from))
        .await
        .map_err(Into::into)
}
