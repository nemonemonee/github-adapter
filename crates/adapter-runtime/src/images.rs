//! Optional image provider. One explicit POST; downloads never inherit credentials.
use crate::{context::RequestContext, transport};
use adapter_protocol::{AdapterError, Result, Value};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    io::Cursor,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};
use url::Url;

const IMAGE_BYTES: usize = 32 * 1024 * 1024;
const JSON_BYTES: usize = 48 * 1024 * 1024;
const OPENAI_SIZES: &[&str] = &["1024x1024", "1536x1024", "1024x1536"];
const FIXED_QWEN_SIZES: &[&str] = &[
    "1328x1328",
    "1664x928",
    "928x1664",
    "1472x1104",
    "1104x1472",
];

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ImageProtocol {
    OpenaiImages,
    QwenMessages,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    pub endpoint: String,
    pub model: String,
    pub protocol: ImageProtocol,
}
impl ImageConfig {
    /// A deliberately bounded size set, not a claim of every upstream capability.
    pub fn supported_sizes(&self) -> &'static [&'static str] {
        // Alibaba's Max/Plus models use a fixed grid, unlike Qwen 2.0.
        // https://www.alibabacloud.com/help/en/model-studio/qwen-image-api
        if self.protocol == ImageProtocol::QwenMessages
            && (matches!(
                self.model.as_str(),
                "qwen-image" | "qwen-image-max" | "qwen-image-plus"
            ) || self.model.starts_with("qwen-image-max-")
                || self.model.starts_with("qwen-image-plus-"))
        {
            FIXED_QWEN_SIZES
        } else {
            OPENAI_SIZES
        }
    }

    pub fn validate(&self) -> Result<()> {
        let url = transport::endpoint(&self.endpoint)
            .map_err(|_| invalid("Image endpoint must be an unambiguous HTTPS URL without credentials, query or fragment."))?;
        if url.scheme() != "https" || self.endpoint.len() > 2048 {
            return Err(invalid(
                "Image endpoint must use HTTPS without credentials, query or fragment.",
            ));
        }
        if self.model.trim().is_empty()
            || self.model.len() > 256
            || self.model.chars().any(char::is_control)
        {
            return Err(invalid("The image provider needs an explicit model ID."));
        }
        Ok(())
    }
}

pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
}
impl GeneratedImage {
    pub fn content(&self) -> Value {
        json!({"type":"image", "mimeType":self.mime_type, "data":STANDARD.encode(&self.bytes)})
    }
}
fn invalid(message: &str) -> AdapterError {
    AdapterError::invalid(message)
}
fn failed(message: &str) -> AdapterError {
    AdapterError::upstream(message)
}

fn body(config: &ImageConfig, prompt: &str, size: &str) -> Result<Value> {
    if prompt.trim().is_empty() || prompt.chars().count() > 16_000 || prompt.len() > 64 * 1024 {
        return Err(invalid(
            "An image prompt must contain 1 to 16000 characters (at most 64 KiB).",
        ));
    }
    let sizes = config.supported_sizes();
    let size = if size == "auto" { sizes[0] } else { size };
    if !sizes.contains(&size) {
        return Err(invalid(
            "Unsupported image dimensions for the configured provider/model. Use auto or a listed supported size.",
        ));
    }
    Ok(match config.protocol {
        ImageProtocol::OpenaiImages => {
            json!({"model":config.model,"prompt":prompt,"n":1,"size":size})
        }
        ImageProtocol::QwenMessages => {
            json!({"model":config.model,"input":{"messages":[{"role":"user","content":[{"text":prompt}]}]},"parameters":{"n":1,"size":size.replace('x',"*"),"watermark":false}})
        }
    })
}

pub async fn generate(
    config: &ImageConfig,
    key: &str,
    prompt: &str,
    size: &str,
    ctx: RequestContext,
) -> Result<GeneratedImage> {
    config.validate()?;
    generate_with_client(config, key, prompt, size, ctx, transport::client(false)?).await
}

async fn generate_with_client(
    config: &ImageConfig,
    key: &str,
    prompt: &str,
    size: &str,
    ctx: RequestContext,
    client: reqwest::Client,
) -> Result<GeneratedImage> {
    let _guard = ctx.cancel_on_drop();
    if key.is_empty() || key.len() > 8192 || key.chars().any(char::is_control) {
        return Err(invalid("The configured image credential is invalid."));
    }
    let payload = body(config, prompt, size)?;
    let request = client
        .post(&config.endpoint)
        .header("authorization", transport::bearer("Bearer", key)?)
        .header("accept", "application/json")
        .header("accept-encoding", "identity")
        .header("content-type", "application/json")
        .body(transport::encode(&payload, 128 * 1024)?)
        .timeout(transport::timeout(&ctx, Some(Duration::from_secs(180)))?);
    let response = transport::send(request, &ctx, "Image generation").await?;
    if !response.status().is_success() {
        return Err(AdapterError::new(
            transport::error_status(response.status().as_u16()),
            "image_provider_failed",
            "The configured image provider rejected the request. It was not retried.",
        ));
    }
    let value = transport::read_json(response, &ctx, JSON_BYTES, "Image generation").await?;
    if value.get("error").is_some_and(|v| !v.is_null()) {
        return Err(failed(
            "Image generation failed. Do not repeat it automatically.",
        ));
    }
    let bytes = if let Some(data) = value["data"].as_array() {
        if data.len() != 1 {
            return Err(failed("The provider must return exactly one image."));
        }
        if let Some(encoded) = data[0]["b64_json"].as_str() {
            if encoded.len() > IMAGE_BYTES * 4 / 3 + 4 {
                return Err(failed("Generated image exceeds the byte limit."));
            }
            STANDARD
                .decode(encoded)
                .map_err(|_| failed("The provider returned invalid base64 image data."))?
        } else if let Some(url) = data[0]["url"].as_str() {
            download(url, &ctx).await?
        } else {
            return Err(failed("The provider returned no image data."));
        }
    } else {
        let contents = value["output"]["choices"][0]["message"]["content"]
            .as_array()
            .ok_or_else(|| failed("The provider returned no image data."))?;
        let urls: Vec<_> = contents
            .iter()
            .filter_map(|item| item["image"].as_str())
            .collect();
        if urls.len() != 1 {
            return Err(failed("The provider must return exactly one image."));
        }
        download(urls[0], &ctx).await?
    };
    ctx.check()?;
    let image = validate_image(bytes)?;
    ctx.check()?;
    Ok(image)
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 168 || (b == 0 && matches!(c, 0 | 2)) || (b == 88 && c == 99)))
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113))
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = v6.segments();
            (s[0] & 0xe000 == 0x2000)
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
        }
    }
}
async fn download(raw: &str, ctx: &RequestContext) -> Result<Vec<u8>> {
    let url = Url::parse(raw).map_err(|_| failed("The image download URL is invalid."))?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(failed(
            "Generated images must be downloaded from public HTTPS without user credentials.",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| failed("The image download has no host."))?;
    let addresses: Vec<SocketAddr> = ctx
        .bounded(async {
            Ok(tokio::net::lookup_host((host, 443))
                .await
                .map_err(|_| failed("Image download DNS lookup failed."))?
                .collect())
        })
        .await?;
    if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
        return Err(failed(
            "Private, local or reserved image download addresses are not allowed.",
        ));
    }
    // Resolve once, pin those addresses, retain TLS hostname verification, and do
    // not let a proxy or redirect bypass the public-address policy. No API key here.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .resolve_to_addrs(host, &addresses)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| failed("Could not create the image download client."))?;
    let response = transport::send(
        client
            .get(url)
            .timeout(transport::timeout(ctx, Some(Duration::from_secs(60)))?),
        ctx,
        "Image download",
    )
    .await?;
    if !response.status().is_success() {
        return Err(failed(
            "Image download failed. Generation was not repeated.",
        ));
    }
    let mut stream = transport::body(response, ctx.clone(), IMAGE_BYTES, "Image download")?;
    let mut bytes = Vec::new();
    while let Some(part) = stream.next().await {
        bytes.extend_from_slice(&part?);
    }
    Ok(bytes)
}
fn validate_image(bytes: Vec<u8>) -> Result<GeneratedImage> {
    if bytes.is_empty() || bytes.len() > IMAGE_BYTES {
        return Err(failed(
            "Generated image is empty or exceeds its byte limit.",
        ));
    }
    let format = image::guess_format(&bytes)
        .map_err(|_| failed("The provider did not return a supported image."))?;
    let mime_type = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::WebP => "image/webp",
        _ => return Err(failed("Only PNG, JPEG and WebP images are supported.")),
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|_| failed("The generated image is corrupt or exceeds decoding limits."))?;
    Ok(GeneratedImage {
        width: decoded.width(),
        height: decoded.height(),
        bytes,
        mime_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Fixture, Reply};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    fn png() -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }
    #[test]
    fn download_address_policy_blocks_private_and_transition_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "192.168.1.1",
            "198.19.0.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2002:7f00:1::",
            "2001:db8::1",
            "fc00::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
    #[test]
    fn image_validation_and_preflight_are_bounded() {
        assert_eq!(validate_image(png()).unwrap().width, 1);
        assert!(validate_image(b"not a png".to_vec()).is_err());
        let c = ImageConfig {
            endpoint: "https://example.com/v1/images/generations".into(),
            model: "explicit-model".into(),
            protocol: ImageProtocol::OpenaiImages,
        };
        assert!(c.validate().is_ok());
        assert!(body(&c, "", "1024x1024").is_err());
        assert!(body(&c, "prompt", "4096x4096").is_err());
        for endpoint in [
            "http://localhost/x",
            "https://user:secret@example.com/x",
            "https://example.com/x?key=secret",
            "https://example.com/x#secret",
        ] {
            assert!(
                ImageConfig {
                    endpoint: endpoint.into(),
                    ..c.clone()
                }
                .validate()
                .is_err()
            );
        }
        let q = ImageConfig {
            protocol: ImageProtocol::QwenMessages,
            ..c
        };
        assert_eq!(
            body(&q, "prompt", "1536x1024").unwrap()["parameters"]["size"],
            "1536*1024"
        );
    }
    #[test]
    fn configured_image_endpoints_share_the_unambiguous_transport_policy() {
        let config = ImageConfig {
            endpoint: "https://images.example.invalid/v1/images/generations".into(),
            model: "synthetic-model".into(),
            protocol: ImageProtocol::OpenaiImages,
        };
        assert!(config.validate().is_ok());
        for endpoint in [
            " https://images.example.invalid/v1/images/generations",
            "https://images.example.invalid/v1/images/generations ",
            "https://images.example.invalid/\timages",
            "https://images.example.invalid/\nimages",
            "https://images.example.invalid:0/images",
            "https://images.example.invalid/v1/../images",
            "https://images.example.invalid/v1/%2e%2e/images",
            "https://images.example.invalid/v1%2fimages",
            "https://images.example.invalid/v1%5cimages",
            "https://images.example.invalid/v1%252fimages",
            r"https://images.example.invalid\images",
        ] {
            assert!(
                ImageConfig {
                    endpoint: endpoint.into(),
                    ..config.clone()
                }
                .validate()
                .is_err(),
                "{endpoint:?}"
            );
        }
    }

    #[tokio::test]
    async fn one_generation_and_no_retry_or_private_download() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let fixture = Fixture::start(move |request, _| {
            assert_eq!(
                request.headers.get("authorization").unwrap(),
                "Bearer image-only-secret"
            );
            assert_eq!(request.json()["n"], 1);
            match count.fetch_add(1, Ordering::SeqCst) {
                0 => Reply::json(json!({"data":[{"b64_json":STANDARD.encode(png())}]})),
                1 => {
                    Reply::json(json!({"error":{"message":"secret image-only-secret"}})).status(500)
                }
                _ => Reply::json(json!({"data":[{"url":"https://127.0.0.1/private"}]})),
            }
        })
        .await;
        let config = ImageConfig {
            endpoint: fixture
                .origin
                .join("images/generations")
                .unwrap()
                .to_string(),
            model: "synthetic".into(),
            protocol: ImageProtocol::OpenaiImages,
        };
        for i in 0..3 {
            let result = generate_with_client(
                &config,
                "image-only-secret",
                "synthetic pixels",
                "1024x1024",
                RequestContext::new(Duration::from_secs(5)).unwrap(),
                transport::client(true).unwrap(),
            )
            .await;
            if i == 0 {
                assert_eq!(result.unwrap().mime_type, "image/png");
            } else {
                let error = result.err().unwrap();
                assert!(!error.message.contains("image-only-secret"));
            }
            assert_eq!(calls.load(Ordering::SeqCst), i + 1);
        }
    }
    #[test]
    fn download_address_policy_distinguishes_public_192_zero_from_reserved_subnets() {
        for ip in [
            "192.0.78.24",
            "192.0.79.24",
            "192.0.1.1",
            "::ffff:192.0.78.24",
        ] {
            assert!(
                public_ip(ip.parse().unwrap()),
                "Public address incorrectly blocked: {ip}"
            );
        }
        for ip in [
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.2",
            "192.168.1.1",
            "::ffff:192.0.2.1",
        ] {
            assert!(
                !public_ip(ip.parse().unwrap()),
                "Special-use address incorrectly allowed: {ip}"
            );
        }
    }

    #[test]
    fn fixed_qwen_families_have_valid_defaults_and_preflight_sizes() {
        for model in [
            "qwen-image",
            "qwen-image-max",
            "qwen-image-max-2025-12-30",
            "qwen-image-plus",
            "qwen-image-plus-2026-01-09",
        ] {
            let config = ImageConfig {
                endpoint: "https://images.example.invalid/generation".into(),
                model: model.into(),
                protocol: ImageProtocol::QwenMessages,
            };
            assert_eq!(
                body(&config, "synthetic prompt", "auto").unwrap()["parameters"]["size"],
                "1328*1328"
            );
            for size in [
                "1328x1328",
                "1664x928",
                "928x1664",
                "1472x1104",
                "1104x1472",
            ] {
                let request = body(&config, "synthetic prompt", size).unwrap();
                assert_eq!(request["parameters"]["n"], 1);
                assert_eq!(request["parameters"]["size"], size.replace('x', "*"));
            }
            for size in ["1024x1024", "1536x1024", "1024x1536", "4096x4096"] {
                assert!(
                    body(&config, "synthetic prompt", size).is_err(),
                    "Unsupported size must fail before dispatch: {model}, {size}"
                );
            }
        }
        for protocol in [ImageProtocol::OpenaiImages, ImageProtocol::QwenMessages] {
            let config = ImageConfig {
                endpoint: "https://images.example.invalid/generation".into(),
                model: "qwen-image-2.0".into(),
                protocol,
            };
            let request = body(&config, "synthetic prompt", "auto").unwrap();
            assert_eq!(
                if protocol == ImageProtocol::OpenaiImages {
                    &request["size"]
                } else {
                    &request["parameters"]["size"]
                },
                if protocol == ImageProtocol::OpenaiImages {
                    "1024x1024"
                } else {
                    "1024*1024"
                }
            );
        }
    }
}
