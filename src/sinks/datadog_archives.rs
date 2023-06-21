// NOTE: We intentionally do not assert/verify that `datadog_archives` meets the component specification because it
// derives all of its capabilities from existing sink implementations which themselves are tested. We probably _should_
// also verify it here, but for now, this is a punt to avoid having to add a bunch of specific integration tests that
// exercise all possible configurations of the sink.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryFrom,
    io::{self, Write},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use azure_storage_blobs::prelude::ContainerClient;
use base64::prelude::{Engine as _, BASE64_STANDARD};
use bytes::{BufMut, Bytes, BytesMut};
use chrono::{SecondsFormat, Utc};
use codecs::{encoding::Framer, JsonSerializerConfig, NewlineDelimitedEncoder};
use goauth::scopes::Scope;
use http::header::{HeaderName, HeaderValue};
use http::Uri;
use lookup::event_path;
use rand::{thread_rng, Rng};
use snafu::Snafu;
use tower::ServiceBuilder;
use uuid::Uuid;
use vector_common::request_metadata::RequestMetadata;
use vector_config::{configurable_component, NamedComponent};
use vector_core::{
    config::AcknowledgementsConfig,
    event::{Event, EventFinalizers, Finalizable},
    schema, EstimatedJsonEncodedSizeOf,
};
use vrl::value::Kind;

use crate::{
    aws::{AwsAuthentication, RegionOrEndpoint},
    codecs::{Encoder, Transformer},
    config::{GenerateConfig, Input, SinkConfig, SinkContext},
    gcp::{GcpAuthConfig, GcpAuthenticator},
    http::{get_http_scheme_from_uri, HttpClient},
    serde::json::to_string,
    sinks::{
        azure_common::{
            self,
            config::{AzureBlobMetadata, AzureBlobRequest, AzureBlobRetryLogic},
            service::AzureBlobService,
            sink::AzureBlobSink,
        },
        gcs_common::{
            self,
            config::{GcsPredefinedAcl, GcsRetryLogic, GcsStorageClass, BASE_URL},
            service::{GcsRequest, GcsRequestSettings, GcsService},
            sink::GcsSink,
        },
        s3_common::{
            self,
            config::{
                create_service, S3CannedAcl, S3RetryLogic, S3ServerSideEncryption, S3StorageClass,
            },
            partitioner::{S3KeyPartitioner, S3PartitionKey},
            service::{S3Metadata, S3Request, S3Service},
            sink::S3Sink,
        },
        util::{
            metadata::RequestMetadataBuilder, partitioner::KeyPartitioner,
            request_builder::EncodeResult, BatchConfig, Compression, RequestBuilder,
            ServiceBuilderExt, SinkBatchSettings, TowerRequestConfig,
        },
        VectorSink,
    },
    template::Template,
    tls::{TlsConfig, TlsSettings},
};

const DEFAULT_COMPRESSION: Compression = Compression::gzip_default();

#[derive(Clone, Copy, Debug, Default)]
pub struct DatadogArchivesDefaultBatchSettings;

/// We should avoid producing many small batches - this might slow down Log Rehydration,
/// these values are similar with how DataDog's Log Archives work internally:
/// batch size - 100mb
/// batch timeout - 15min
impl SinkBatchSettings for DatadogArchivesDefaultBatchSettings {
    const MAX_EVENTS: Option<usize> = None;
    const MAX_BYTES: Option<usize> = Some(100_000_000);
    const TIMEOUT_SECS: f64 = 900.0;
}
/// Configuration for the `datadog_archives` sink.
#[configurable_component]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DatadogArchivesSinkConfig {
    /// The name of the object storage service to use.
    // TODO: This should really be an enum.
    pub service: String,

    /// The name of the bucket to store the archives in.
    pub bucket: String,

    /// A prefix to apply to all object keys.
    ///
    /// Prefixes are useful for partitioning objects, such as by creating an object key that
    /// stores objects under a particular directory. If using a prefix for this purpose, it must end
    /// in `/` to act as a directory path. A trailing `/` is **not** automatically added.
    pub key_prefix: Option<String>,

    #[configurable(derived)]
    #[serde(default)]
    pub request: TowerRequestConfig,

    #[configurable(derived)]
    #[serde(default)]
    pub aws_s3: Option<S3Config>,

    #[configurable(derived)]
    #[serde(default)]
    pub azure_blob: Option<AzureBlobConfig>,

    #[configurable(derived)]
    #[serde(default)]
    pub gcp_cloud_storage: Option<GcsConfig>,

    #[configurable(derived)]
    tls: Option<TlsConfig>,

    #[configurable(derived)]
    #[serde(
        default,
        skip_serializing_if = "crate::serde::skip_serializing_if_default"
    )]
    pub encoding: Transformer,

    #[configurable(derived)]
    #[serde(
        default,
        deserialize_with = "crate::serde::bool_or_struct",
        skip_serializing_if = "crate::serde::skip_serializing_if_default"
    )]
    acknowledgements: AcknowledgementsConfig,
}

/// S3-specific configuration options.
#[configurable_component]
#[derive(Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    #[serde(flatten)]
    pub options: S3Options,

    #[serde(flatten)]
    pub region: RegionOrEndpoint,

    #[configurable(derived)]
    #[serde(default)]
    pub auth: AwsAuthentication,
}

/// S3-specific bucket/object options.
#[configurable_component]
#[derive(Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct S3Options {
    /// Canned ACL to apply to the created objects.
    ///
    /// For more information, see [Canned ACL][canned_acl].
    ///
    /// [canned_acl]: https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#canned-acl
    pub acl: Option<S3CannedAcl>,

    /// Grants `READ`, `READ_ACP`, and `WRITE_ACP` permissions on the created objects to the named [grantee].
    ///
    /// This allows the grantee to read the created objects and their metadata, as well as read and
    /// modify the ACL on the created objects.
    ///
    /// [grantee]: https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#specifying-grantee
    pub grant_full_control: Option<String>,

    /// Grants `READ` permissions on the created objects to the named [grantee].
    ///
    /// This allows the grantee to read the created objects and their metadata.
    ///
    /// [grantee]: https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#specifying-grantee
    pub grant_read: Option<String>,

    /// Grants `READ_ACP` permissions on the created objects to the named [grantee].
    ///
    /// This allows the grantee to read the ACL on the created objects.
    ///
    /// [grantee]: https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#specifying-grantee
    pub grant_read_acp: Option<String>,

    /// Grants `WRITE_ACP` permissions on the created objects to the named [grantee].
    ///
    /// This allows the grantee to modify the ACL on the created objects.
    ///
    /// [grantee]: https://docs.aws.amazon.com/AmazonS3/latest/dev/acl-overview.html#specifying-grantee
    pub grant_write_acp: Option<String>,

    /// The Server-side Encryption algorithm used when storing these objects.
    pub server_side_encryption: Option<S3ServerSideEncryption>,

    /// Specifies the ID of the AWS Key Management Service (AWS KMS) symmetrical customer managed
    /// customer master key (CMK) that is used for the created objects.
    ///
    /// Only applies when `server_side_encryption` is configured to use KMS.
    ///
    /// If not specified, Amazon S3 uses the AWS managed CMK in AWS to protect the data.
    pub ssekms_key_id: Option<String>,

    /// The storage class for the created objects.
    ///
    /// For more information, see [Using Amazon S3 storage classes][storage_classes].
    ///
    /// [storage_classes]: https://docs.aws.amazon.com/AmazonS3/latest/dev/storage-class-intro.html
    pub storage_class: S3StorageClass,

    /// The tag-set for the object.
    #[configurable(metadata(docs::additional_props_description = "A single tag."))]
    pub tags: Option<BTreeMap<String, String>>,
}

/// ABS-specific configuration options.
#[configurable_component]
#[derive(Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct AzureBlobConfig {
    /// The Azure Blob Storage Account connection string.
    ///
    /// Authentication with access key is the only supported authentication method.
    pub connection_string: String,
}

/// GCS-specific configuration options.
#[configurable_component]
#[derive(Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct GcsConfig {
    #[configurable(derived)]
    acl: Option<GcsPredefinedAcl>,

    #[configurable(derived)]
    storage_class: Option<GcsStorageClass>,

    /// The set of metadata `key:value` pairs for the created objects.
    ///
    /// For more information, see [Custom metadata][custom_metadata].
    ///
    /// [custom_metadata]: https://cloud.google.com/storage/docs/metadata#custom-metadata
    #[configurable(metadata(docs::additional_props_description = "A key/value pair."))]
    metadata: Option<HashMap<String, String>>,

    #[serde(flatten)]
    auth: GcpAuthConfig,
}

impl GenerateConfig for DatadogArchivesSinkConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            service: "".to_owned(),
            bucket: "".to_owned(),
            key_prefix: None,
            request: TowerRequestConfig::default(),
            aws_s3: None,
            gcp_cloud_storage: None,
            tls: None,
            azure_blob: None,
            encoding: Default::default(),
            acknowledgements: Default::default(),
        })
        .unwrap()
    }
}

#[derive(Debug, Snafu, PartialEq)]
enum ConfigError {
    #[snafu(display("Unsupported service: {}", service))]
    UnsupportedService { service: String },
    #[snafu(display("Unsupported storage class: {}", storage_class))]
    UnsupportedStorageClass { storage_class: String },
}

const KEY_TEMPLATE: &str = "/dt=%Y%m%d/hour=%H/";

impl DatadogArchivesSinkConfig {
    async fn build_sink(&self, cx: SinkContext) -> crate::Result<(VectorSink, super::Healthcheck)> {
        match &self.service[..] {
            "aws_s3" => {
                let s3_config = self.aws_s3.as_ref().expect("s3 config wasn't provided");
                let service =
                    create_service(&s3_config.region, &s3_config.auth, &cx.proxy, &self.tls)
                        .await?;
                let client = service.client();
                let svc = self
                    .build_s3_sink(&s3_config.options, service)
                    .map_err(|error| error.to_string())?;
                Ok((
                    svc,
                    s3_common::config::build_healthcheck(self.bucket.clone(), client)?,
                ))
            }
            "azure_blob" => {
                let azure_config = self
                    .azure_blob
                    .as_ref()
                    .expect("azure blob config wasn't provided");
                let client = azure_common::config::build_client(
                    Some(azure_config.connection_string.clone()),
                    None,
                    self.bucket.clone(),
                    None,
                )?;
                let svc = self
                    .build_azure_sink(Arc::<ContainerClient>::clone(&client))
                    .map_err(|error| error.to_string())?;
                let healthcheck =
                    azure_common::config::build_healthcheck(self.bucket.clone(), client)?;
                Ok((svc, healthcheck))
            }
            "gcp_cloud_storage" => {
                let gcs_config = self
                    .gcp_cloud_storage
                    .as_ref()
                    .expect("gcs config wasn't provided");
                let auth = gcs_config.auth.build(Scope::DevStorageReadWrite).await?;
                let base_url = format!("{}{}/", BASE_URL, self.bucket);
                let tls = TlsSettings::from_options(&self.tls)?;
                let client = HttpClient::new(tls, cx.proxy())?;
                let healthcheck = gcs_common::config::build_healthcheck(
                    self.bucket.clone(),
                    client.clone(),
                    base_url.clone(),
                    auth.clone(),
                )?;
                let sink = self
                    .build_gcs_sink(client, base_url, auth)
                    .map_err(|error| error.to_string())?;
                Ok((sink, healthcheck))
            }

            service => Err(Box::new(ConfigError::UnsupportedService {
                service: service.to_owned(),
            })),
        }
    }

    fn build_s3_sink(
        &self,
        s3_options: &S3Options,
        service: S3Service,
    ) -> Result<VectorSink, ConfigError> {
        // we use lower default limits, because we send 100mb batches,
        // thus no need of the higher number of outgoing requests
        let request_limits = self.request.unwrap_with(&Default::default());
        let service = ServiceBuilder::new()
            .settings(request_limits, S3RetryLogic)
            .service(service);

        match s3_options.storage_class {
            class @ S3StorageClass::DeepArchive | class @ S3StorageClass::Glacier => {
                return Err(ConfigError::UnsupportedStorageClass {
                    storage_class: format!("{:?}", class),
                });
            }
            _ => (),
        }

        let batcher_settings = BatchConfig::<DatadogArchivesDefaultBatchSettings>::default()
            .into_batcher_settings()
            .expect("invalid batch settings");

        let partitioner = S3KeyPartitioner::new(
            Template::try_from(KEY_TEMPLATE).expect("invalid object key format"),
            None,
        );

        let s3_config = self
            .aws_s3
            .as_ref()
            .expect("s3 config wasn't provided")
            .clone();
        let request_builder = DatadogS3RequestBuilder::new(
            self.bucket.clone(),
            self.key_prefix.clone(),
            s3_config,
            self.encoding.clone(),
        );

        let sink = S3Sink::new(service, request_builder, partitioner, batcher_settings);

        Ok(VectorSink::from_event_streamsink(sink))
    }

    pub fn build_gcs_sink(
        &self,
        client: HttpClient,
        base_url: String,
        auth: GcpAuthenticator,
    ) -> crate::Result<VectorSink> {
        let request = self.request.unwrap_with(&Default::default());
        let protocol = get_http_scheme_from_uri(&base_url.parse::<Uri>()?);

        let batcher_settings = BatchConfig::<DatadogArchivesDefaultBatchSettings>::default()
            .into_batcher_settings()
            .expect("invalid batch settings");

        let svc = ServiceBuilder::new()
            .settings(request, GcsRetryLogic)
            .service(GcsService::new(client, base_url, auth));

        let gcs_config = self
            .gcp_cloud_storage
            .as_ref()
            .expect("gcs config wasn't provided")
            .clone();

        let acl = gcs_config
            .acl
            .map(|acl| HeaderValue::from_str(&to_string(acl)).unwrap());
        let storage_class = gcs_config.storage_class.unwrap_or_default();
        let storage_class = HeaderValue::from_str(&to_string(storage_class)).unwrap();
        let metadata = gcs_config
            .metadata
            .as_ref()
            .map(|metadata| {
                metadata
                    .iter()
                    .map(make_header)
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_else(|| Ok(vec![]))?;
        let request_builder = DatadogGcsRequestBuilder {
            bucket: self.bucket.clone(),
            key_prefix: self.key_prefix.clone(),
            acl,
            storage_class,
            metadata,
            encoding: DatadogArchivesEncoding::new(self.encoding.clone()),
            compression: DEFAULT_COMPRESSION,
        };

        let partitioner = DatadogArchivesSinkConfig::build_partitioner();

        let sink = GcsSink::new(
            svc,
            request_builder,
            partitioner,
            batcher_settings,
            protocol,
        );

        Ok(VectorSink::from_event_streamsink(sink))
    }

    fn build_azure_sink(&self, client: Arc<ContainerClient>) -> crate::Result<VectorSink> {
        let request_limits = self.request.unwrap_with(&Default::default());
        let service = ServiceBuilder::new()
            .settings(request_limits, AzureBlobRetryLogic)
            .service(AzureBlobService::new(client));

        let batcher_settings = BatchConfig::<DatadogArchivesDefaultBatchSettings>::default()
            .into_batcher_settings()
            .expect("invalid batch settings");

        let partitioner = DatadogArchivesSinkConfig::build_partitioner();
        let request_builder = DatadogAzureRequestBuilder {
            container_name: self.bucket.clone(),
            blob_prefix: self.key_prefix.clone(),
            encoding: DatadogArchivesEncoding::new(self.encoding.clone()),
        };

        let sink = AzureBlobSink::new(service, request_builder, partitioner, batcher_settings);

        Ok(VectorSink::from_event_streamsink(sink))
    }

    pub fn build_partitioner() -> KeyPartitioner {
        KeyPartitioner::new(Template::try_from(KEY_TEMPLATE).expect("invalid object key format"))
    }
}

const RESERVED_ATTRIBUTES: [&str; 10] = [
    "_id", "date", "message", "host", "source", "service", "status", "tags", "trace_id", "span_id",
];

#[derive(Debug)]
struct DatadogArchivesEncoding {
    encoder: (Transformer, Encoder<Framer>),
    reserved_attributes: HashSet<&'static str>,
    id_rnd_bytes: [u8; 8],
    id_seq_number: AtomicU32,
}

impl DatadogArchivesEncoding {
    /// Generates a unique event ID compatible with DD:
    /// - 18 bytes;
    /// - first 6 bytes represent a "now" timestamp in millis;
    /// - the rest 12 bytes can be just any sequence unique for a given timestamp.
    ///
    /// To generate unique-ish trailing 12 bytes we use random 8 bytes, generated at startup,
    /// and a rolling-over 4-bytes sequence number.
    fn generate_log_id(&self) -> String {
        let mut id = BytesMut::with_capacity(18);
        // timestamp in millis - 6 bytes
        let now = Utc::now();
        id.put_int(now.timestamp_millis(), 6);

        // 8 random bytes
        id.put_slice(&self.id_rnd_bytes);

        // 4 bytes for the counter should be more than enough - it should be unique for 1 millisecond only
        let id_seq_number = self.id_seq_number.fetch_add(1, Ordering::Relaxed);
        id.put_u32(id_seq_number);

        BASE64_STANDARD.encode(id.freeze())
    }
}

impl DatadogArchivesEncoding {
    pub fn new(transformer: Transformer) -> Self {
        Self {
            encoder: (
                transformer,
                Encoder::<Framer>::new(
                    NewlineDelimitedEncoder::new().into(),
                    JsonSerializerConfig::default().build().into(),
                ),
            ),
            reserved_attributes: RESERVED_ATTRIBUTES.iter().copied().collect(),
            id_rnd_bytes: thread_rng().gen::<[u8; 8]>(),
            id_seq_number: AtomicU32::new(0),
        }
    }
}

impl crate::sinks::util::encoding::Encoder<Vec<Event>> for DatadogArchivesEncoding {
    /// Applies the following transformations to align event's schema with DD:
    /// - (required) `_id` is generated in the sink(format described below);
    /// - (required) `date` is set from the `timestamp` meaning or Global Log Schema mapping, or to the current time if missing;
    /// - `message`,`host` are set from the corresponding meanings or Global Log Schema mappings;
    /// - `source`, `service`, `status`, `tags` and other reserved attributes are left as is;
    /// - the rest of the fields is moved to `attributes`.
    // TODO: All reserved attributes could have specific meanings, rather than specific paths
    fn encode_input(&self, mut input: Vec<Event>, writer: &mut dyn Write) -> io::Result<usize> {
        for event in input.iter_mut() {
            let log_event = event.as_mut_log();

            log_event.insert("_id", self.generate_log_id());

            let timestamp = log_event
                .remove_timestamp()
                .unwrap_or_else(|| Utc::now().timestamp_millis().into());
            log_event.insert(
                "date",
                timestamp
                    .as_timestamp()
                    .cloned()
                    .unwrap_or_else(Utc::now)
                    .to_rfc3339_opts(SecondsFormat::Millis, true),
            );

            if let Some(message_path) = log_event.message_path() {
                log_event.rename_key(message_path.as_str(), event_path!("message"));
            }

            if let Some(host_path) = log_event.host_path() {
                log_event.rename_key(host_path.as_str(), event_path!("host"));
            }

            let mut attributes = BTreeMap::new();

            let custom_attributes = if let Some(map) = log_event.as_map() {
                map.keys()
                    .filter(|&path| !self.reserved_attributes.contains(path.as_str()))
                    .map(|v| v.to_owned())
                    .collect()
            } else {
                vec![]
            };

            for path in custom_attributes {
                if let Some(value) = log_event.remove(path.as_str()) {
                    attributes.insert(path, value);
                }
            }
            log_event.insert("attributes", attributes);
        }

        self.encoder.encode_input(input, writer)
    }
}
#[derive(Debug)]
struct DatadogS3RequestBuilder {
    bucket: String,
    key_prefix: Option<String>,
    config: S3Config,
    encoding: DatadogArchivesEncoding,
}

impl DatadogS3RequestBuilder {
    pub fn new(
        bucket: String,
        key_prefix: Option<String>,
        config: S3Config,
        transformer: Transformer,
    ) -> Self {
        Self {
            bucket,
            key_prefix,
            config,
            encoding: DatadogArchivesEncoding::new(transformer),
        }
    }
}

impl RequestBuilder<(S3PartitionKey, Vec<Event>)> for DatadogS3RequestBuilder {
    type Metadata = S3Metadata;
    type Events = Vec<Event>;
    type Encoder = DatadogArchivesEncoding;
    type Payload = Bytes;
    type Request = S3Request;
    type Error = io::Error;

    fn compression(&self) -> Compression {
        DEFAULT_COMPRESSION
    }

    fn encoder(&self) -> &Self::Encoder {
        &self.encoding
    }

    fn split_input(
        &self,
        input: (S3PartitionKey, Vec<Event>),
    ) -> (Self::Metadata, RequestMetadataBuilder, Self::Events) {
        let (partition_key, mut events) = input;
        let finalizers = events.take_finalizers();
        let s3_key_prefix = partition_key.key_prefix.clone();

        let builder = RequestMetadataBuilder::from_events(&events);

        let s3metadata = S3Metadata {
            partition_key,
            s3_key: s3_key_prefix,
            finalizers,
        };

        (s3metadata, builder, events)
    }

    fn build_request(
        &self,
        mut metadata: Self::Metadata,
        request_metadata: RequestMetadata,
        payload: EncodeResult<Self::Payload>,
    ) -> Self::Request {
        metadata.s3_key = generate_object_key(self.key_prefix.clone(), metadata.s3_key);

        let body = payload.into_payload();
        trace!(
            message = "Sending events.",
            bytes = ?body.len(),
            events_len = ?request_metadata.events_byte_size(),
            bucket = ?self.bucket,
            key = ?metadata.partition_key
        );

        let s3_options = self.config.options.clone();
        S3Request {
            body,
            bucket: self.bucket.clone(),
            metadata,
            request_metadata,
            content_encoding: DEFAULT_COMPRESSION.content_encoding(),
            options: s3_common::config::S3Options {
                acl: s3_options.acl,
                grant_full_control: s3_options.grant_full_control,
                grant_read: s3_options.grant_read,
                grant_read_acp: s3_options.grant_read_acp,
                grant_write_acp: s3_options.grant_write_acp,
                server_side_encryption: s3_options.server_side_encryption,
                ssekms_key_id: s3_options.ssekms_key_id,
                storage_class: s3_options.storage_class,
                tags: s3_options.tags.map(|tags| tags.into_iter().collect()),
                content_encoding: None,
                content_type: None,
            },
        }
    }
}

#[derive(Debug)]
struct DatadogGcsRequestBuilder {
    bucket: String,
    key_prefix: Option<String>,
    acl: Option<HeaderValue>,
    storage_class: HeaderValue,
    metadata: Vec<(HeaderName, HeaderValue)>,
    encoding: DatadogArchivesEncoding,
    compression: Compression,
}

impl RequestBuilder<(String, Vec<Event>)> for DatadogGcsRequestBuilder {
    type Metadata = (String, EventFinalizers);
    type Events = Vec<Event>;
    type Payload = Bytes;
    type Request = GcsRequest;
    type Encoder = DatadogArchivesEncoding;
    type Error = io::Error;

    fn split_input(
        &self,
        input: (String, Vec<Event>),
    ) -> (Self::Metadata, RequestMetadataBuilder, Self::Events) {
        let (partition_key, mut events) = input;
        let metadata_builder = RequestMetadataBuilder::from_events(&events);
        let finalizers = events.take_finalizers();

        ((partition_key, finalizers), metadata_builder, events)
    }

    fn build_request(
        &self,
        dd_metadata: Self::Metadata,
        metadata: RequestMetadata,
        payload: EncodeResult<Self::Payload>,
    ) -> Self::Request {
        let (key, finalizers) = dd_metadata;

        let key = generate_object_key(self.key_prefix.clone(), key);

        let body = payload.into_payload();

        trace!(
            message = "Sending events.",
            bytes = body.len(),
            events_len = metadata.event_count(),
            bucket = %self.bucket,
            ?key
        );

        let content_type = HeaderValue::from_str(self.encoding.encoder.1.content_type()).unwrap();
        let content_encoding = DEFAULT_COMPRESSION
            .content_encoding()
            .map(|ce| HeaderValue::from_str(&to_string(ce)).unwrap());

        GcsRequest {
            key,
            body,
            finalizers,
            settings: GcsRequestSettings {
                acl: self.acl.clone(),
                content_type,
                content_encoding,
                storage_class: self.storage_class.clone(),
                headers: self.metadata.clone(),
            },
            metadata,
        }
    }

    fn compression(&self) -> Compression {
        self.compression
    }

    fn encoder(&self) -> &Self::Encoder {
        &self.encoding
    }
}

fn generate_object_key(key_prefix: Option<String>, partition_key: String) -> String {
    let filename = Uuid::new_v4().to_string();

    format!(
        "{}/{}/archive_{}.{}",
        key_prefix.unwrap_or_default(),
        partition_key,
        filename,
        "json.gz"
    )
    .replace("//", "/")
}

#[derive(Debug)]
struct DatadogAzureRequestBuilder {
    container_name: String,
    blob_prefix: Option<String>,
    encoding: DatadogArchivesEncoding,
}

impl RequestBuilder<(String, Vec<Event>)> for DatadogAzureRequestBuilder {
    type Metadata = AzureBlobMetadata;
    type Events = Vec<Event>;
    type Encoder = DatadogArchivesEncoding;
    type Payload = Bytes;
    type Request = AzureBlobRequest;
    type Error = io::Error;

    fn compression(&self) -> Compression {
        DEFAULT_COMPRESSION
    }

    fn encoder(&self) -> &Self::Encoder {
        &self.encoding
    }

    fn split_input(
        &self,
        input: (String, Vec<Event>),
    ) -> (Self::Metadata, RequestMetadataBuilder, Self::Events) {
        let (partition_key, mut events) = input;
        let finalizers = events.take_finalizers();
        let metadata = AzureBlobMetadata {
            partition_key,
            count: events.len(),
            byte_size: events.estimated_json_encoded_size_of(),
            finalizers,
        };
        let builder = RequestMetadataBuilder::from_events(&events);

        (metadata, builder, events)
    }

    fn build_request(
        &self,
        mut metadata: Self::Metadata,
        request_metadata: RequestMetadata,
        payload: EncodeResult<Self::Payload>,
    ) -> Self::Request {
        metadata.partition_key =
            generate_object_key(self.blob_prefix.clone(), metadata.partition_key);

        let blob_data = payload.into_payload();

        trace!(
            message = "Sending events.",
            bytes = ?blob_data.len(),
            events_len = ?metadata.count,
            container = ?self.container_name,
            blob = ?metadata.partition_key
        );

        AzureBlobRequest {
            blob_data,
            content_encoding: DEFAULT_COMPRESSION.content_encoding(),
            content_type: "application/gzip",
            metadata,
            request_metadata,
        }
    }
}

// This is implemented manually to satisfy `SinkConfig`, because if we derive it automatically via
// `#[configurable_component(sink("..."))]`, it would register the sink in a way that allowed it to
// be used in `vector generate`, etc... and we don't want that.
//
// TODO: When the sink is fully supported and we expose it for use/within the docs, remove this.
impl NamedComponent for DatadogArchivesSinkConfig {
    fn get_component_name(&self) -> &'static str {
        "datadog_archives"
    }
}

#[async_trait::async_trait]
impl SinkConfig for DatadogArchivesSinkConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, super::Healthcheck)> {
        let sink_and_healthcheck = self.build_sink(cx).await?;
        Ok(sink_and_healthcheck)
    }

    fn input(&self) -> Input {
        let requirements = schema::Requirement::empty()
            .optional_meaning("host", Kind::bytes())
            .optional_meaning("message", Kind::bytes())
            .optional_meaning("source", Kind::bytes())
            .optional_meaning("service", Kind::bytes())
            .optional_meaning("severity", Kind::bytes())
            // TODO: A `timestamp` is required for rehydration, however today we generate a `Utc::now()`
            // timestamp if it's not found in the event. We could require this meaning instead.
            .optional_meaning("timestamp", Kind::timestamp())
            .optional_meaning("trace_id", Kind::bytes());

        Input::log().with_schema_requirement(requirements)
    }

    fn acknowledgements(&self) -> &AcknowledgementsConfig {
        &self.acknowledgements
    }
}

// Make a header pair from a key-value string pair
fn make_header((name, value): (&String, &String)) -> crate::Result<(HeaderName, HeaderValue)> {
    Ok((
        HeaderName::from_bytes(name.as_bytes())?,
        HeaderValue::from_str(value)?,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::print_stdout)] // tests

    use std::{collections::BTreeMap, io::Cursor};

    use chrono::DateTime;
    use vector_core::partition::Partitioner;

    use super::*;
    use crate::{event::LogEvent, sinks::util::encoding::Encoder as _};

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<DatadogArchivesSinkConfig>();
    }

    #[test]
    fn encodes_event() {
        let mut event = Event::Log(LogEvent::from("test message"));
        let log_mut = event.as_mut_log();
        log_mut.insert("service", "test-service");
        log_mut.insert("not_a_reserved_attribute", "value");
        log_mut.insert("tags", vec!["tag1:value1", "tag2:value2"]);
        let timestamp = DateTime::parse_from_rfc3339("2021-08-23T18:00:27.879+02:00")
            .expect("invalid test case")
            .with_timezone(&Utc);
        log_mut.insert("timestamp", timestamp);

        let mut writer = Cursor::new(Vec::new());
        let encoding = DatadogArchivesEncoding::new(Default::default());
        _ = encoding.encode_input(vec![event], &mut writer);

        let encoded = writer.into_inner();
        let json: BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(encoded.as_slice()).unwrap();

        validate_event_id(
            json.get("_id")
                .expect("_id not found")
                .as_str()
                .expect("_id is not a string"),
        );

        assert_eq!(json.len(), 6); // _id, message, date, service, attributes
        assert_eq!(
            json.get("message")
                .expect("message not found")
                .as_str()
                .expect("message is not a string"),
            "test message"
        );
        assert_eq!(
            json.get("date")
                .expect("date not found")
                .as_str()
                .expect("date is not a string"),
            "2021-08-23T16:00:27.879Z"
        );
        assert_eq!(
            json.get("service")
                .expect("service not found")
                .as_str()
                .expect("service is not a string"),
            "test-service"
        );

        assert_eq!(
            json.get("tags")
                .expect("tags not found")
                .as_array()
                .expect("service is not an array")
                .to_owned(),
            vec!["tag1:value1", "tag2:value2"]
        );

        let attributes = json
            .get("attributes")
            .expect("attributes not found")
            .as_object()
            .expect("attributes is not an object");
        assert_eq!(attributes.len(), 1);
        assert_eq!(
            String::from_utf8_lossy(
                attributes
                    .get("not_a_reserved_attribute")
                    .expect("not_a_reserved_attribute wasn't moved to attributes")
                    .as_str()
                    .expect("not_a_reserved_attribute is not a string")
                    .as_ref()
            ),
            "value"
        );
    }

    #[test]
    fn generates_valid_key_for_an_event() {
        let mut log = LogEvent::from("test message");

        let timestamp = DateTime::parse_from_rfc3339("2021-08-23T18:00:27.879+02:00")
            .expect("invalid test case")
            .with_timezone(&Utc);
        log.insert("timestamp", timestamp);

        let partitioner = DatadogArchivesSinkConfig::build_partitioner();
        let key = partitioner
            .partition(&log.into())
            .expect("key wasn't provided");

        assert_eq!(key, "/dt=20210823/hour=16/");
    }

    #[test]
    fn generates_valid_id() {
        let log1 = Event::Log(LogEvent::from("test event 1"));
        let mut writer = Cursor::new(Vec::new());
        let encoding = DatadogArchivesEncoding::new(Default::default());
        _ = encoding.encode_input(vec![log1], &mut writer);
        let encoded = writer.into_inner();
        let json: BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(encoded.as_slice()).unwrap();
        let id1 = json
            .get("_id")
            .expect("_id not found")
            .as_str()
            .expect("_id is not a string");
        validate_event_id(id1);

        // check that id is different for the next event
        let log2 = Event::Log(LogEvent::from("test event 2"));
        let mut writer = Cursor::new(Vec::new());
        _ = encoding.encode_input(vec![log2], &mut writer);
        let encoded = writer.into_inner();
        let json: BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(encoded.as_slice()).unwrap();
        let id2 = json
            .get("_id")
            .expect("_id not found")
            .as_str()
            .expect("_id is not a string");
        validate_event_id(id2);
        assert_ne!(id1, id2)
    }

    #[test]
    fn generates_date_if_missing() {
        let log = Event::Log(LogEvent::from("test message"));
        let mut writer = Cursor::new(Vec::new());
        let encoding = DatadogArchivesEncoding::new(Default::default());
        _ = encoding.encode_input(vec![log], &mut writer);
        let encoded = writer.into_inner();
        let json: BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(encoded.as_slice()).unwrap();

        let date = DateTime::parse_from_rfc3339(
            json.get("date")
                .expect("date not found")
                .as_str()
                .expect("date is not a string"),
        )
        .expect("date is not in an rfc3339 format");

        // check that it is a recent timestamp
        assert!(Utc::now().timestamp() - date.timestamp() < 1000);
    }

    /// check that _id is:
    /// - 18 bytes,
    /// - base64-encoded,
    /// - first 6 bytes - a "now" timestamp in millis
    fn validate_event_id(id: &str) {
        let bytes = BASE64_STANDARD
            .decode(id)
            .expect("_id is not base64-encoded");
        assert_eq!(bytes.len(), 18);
        let mut timestamp: [u8; 8] = [0; 8];
        for (i, b) in bytes[..6].iter().enumerate() {
            timestamp[i + 2] = *b;
        }
        let timestamp = i64::from_be_bytes(timestamp);
        // check that it is a recent timestamp in millis
        assert!(Utc::now().timestamp_millis() - timestamp < 1000);
    }

    #[test]
    fn s3_build_request() {
        let fake_buf = Bytes::new();
        let mut log = Event::Log(LogEvent::from("test message"));
        let timestamp = DateTime::parse_from_rfc3339("2021-08-23T18:00:27.879+02:00")
            .expect("invalid test case")
            .with_timezone(&Utc);
        log.as_mut_log().insert("timestamp", timestamp);
        let partitioner = S3KeyPartitioner::new(
            Template::try_from(KEY_TEMPLATE).expect("invalid object key format"),
            None,
        );
        let key = partitioner.partition(&log).expect("key wasn't provided");

        let request_builder = DatadogS3RequestBuilder::new(
            "dd-logs".into(),
            Some("audit".into()),
            S3Config::default(),
            Default::default(),
        );

        let (metadata, metadata_request_builder, _events) =
            request_builder.split_input((key, vec![log]));

        let payload = EncodeResult::uncompressed(fake_buf.clone());
        let request_metadata = metadata_request_builder.build(&payload);
        let req = request_builder.build_request(metadata, request_metadata, payload);

        let expected_key_prefix = "audit/dt=20210823/hour=16/archive_";
        let expected_key_ext = ".json.gz";
        println!("{}", req.metadata.s3_key);
        assert!(req.metadata.s3_key.starts_with(expected_key_prefix));
        assert!(req.metadata.s3_key.ends_with(expected_key_ext));
        let uuid1 = &req.metadata.s3_key
            [expected_key_prefix.len()..req.metadata.s3_key.len() - expected_key_ext.len()];
        assert_eq!(uuid1.len(), 36);

        // check that the second batch has a different UUID
        let log2 = LogEvent::default().into();

        let key = partitioner.partition(&log2).expect("key wasn't provided");
        let (metadata, metadata_request_builder, _events) =
            request_builder.split_input((key, vec![log2]));
        let payload = EncodeResult::uncompressed(fake_buf);
        let request_metadata = metadata_request_builder.build(&payload);
        let req = request_builder.build_request(metadata, request_metadata, payload);

        let uuid2 = &req.metadata.s3_key
            [expected_key_prefix.len()..req.metadata.s3_key.len() - expected_key_ext.len()];

        assert_ne!(uuid1, uuid2);
    }

    #[tokio::test]
    async fn error_if_unsupported_s3_storage_class() {
        for (class, supported) in [
            (S3StorageClass::Standard, true),
            (S3StorageClass::StandardIa, true),
            (S3StorageClass::IntelligentTiering, true),
            (S3StorageClass::OnezoneIa, true),
            (S3StorageClass::ReducedRedundancy, true),
            (S3StorageClass::DeepArchive, false),
            (S3StorageClass::Glacier, false),
        ] {
            let config = DatadogArchivesSinkConfig {
                service: "aws_s3".to_owned(),
                bucket: "vector-datadog-archives".to_owned(),
                key_prefix: Some("logs/".to_owned()),
                request: TowerRequestConfig::default(),
                aws_s3: Some(S3Config {
                    options: S3Options {
                        storage_class: class,
                        ..Default::default()
                    },
                    region: RegionOrEndpoint::with_region("us-east-1".to_owned()),
                    auth: Default::default(),
                }),
                azure_blob: None,
                gcp_cloud_storage: None,
                tls: None,
                encoding: Default::default(),
                acknowledgements: Default::default(),
            };

            let res = config.build_sink(SinkContext::new_test()).await;

            if supported {
                assert!(res.is_ok());
            } else {
                assert_eq!(
                    res.err().unwrap().to_string(),
                    format!(r#"Unsupported storage class: {:?}"#, class)
                );
            }
        }
    }
}
