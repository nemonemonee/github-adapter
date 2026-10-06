use adapter_protocol::{AdapterError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Endpoint<'a> {
    Health,
    Models,
    Responses,
    Compaction,
    Chat,
    Messages,
    History(&'a str),
    Unknown,
}

impl<'a> Endpoint<'a> {
    pub(crate) fn classify(path: &'a str) -> Self {
        match path {
            "/" | "/health" | "/health/liveliness" => Self::Health,
            "/models" | "/v1/models" => Self::Models,
            "/responses" | "/v1/responses" => Self::Responses,
            "/responses/compact" | "/v1/responses/compact" => Self::Compaction,
            "/chat/completions" | "/v1/chat/completions" => Self::Chat,
            "/messages" | "/v1/messages" => Self::Messages,
            _ => path
                .strip_prefix("/responses/")
                .or_else(|| path.strip_prefix("/v1/responses/"))
                .filter(|id| !id.is_empty())
                .map_or(Self::Unknown, Self::History),
        }
    }

    pub(crate) fn inference_query_allowed(self, query: &str) -> bool {
        query.is_empty() || (self == Self::Messages && query == "beta=true")
    }

    pub(crate) fn history_id(self) -> Result<Option<String>> {
        // Only GET/DELETE call this projection; POST still selects compaction.
        let id = match self {
            Self::History(id) => id,
            Self::Compaction => "compact",
            _ => return Ok(None),
        };
        let bytes = id.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut position = 0;
        while position < bytes.len() {
            if bytes[position] == b'%' && position + 2 < bytes.len() {
                let hex = |byte: u8| (byte as char).to_digit(16);
                if let (Some(high), Some(low)) =
                    (hex(bytes[position + 1]), hex(bytes[position + 2]))
                {
                    decoded.push((high * 16 + low) as u8);
                    position += 3;
                    continue;
                }
            }
            decoded.push(bytes[position]);
            position += 1;
        }
        String::from_utf8(decoded)
            .map(Some)
            .map_err(|_| AdapterError::invalid("Response history IDs must be valid UTF-8."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_aliases_have_one_classification_and_query_policy() {
        for (left, right, expected) in [
            ("/models", "/v1/models", Endpoint::Models),
            ("/responses", "/v1/responses", Endpoint::Responses),
            (
                "/responses/compact",
                "/v1/responses/compact",
                Endpoint::Compaction,
            ),
            ("/chat/completions", "/v1/chat/completions", Endpoint::Chat),
            ("/messages", "/v1/messages", Endpoint::Messages),
        ] {
            assert_eq!(Endpoint::classify(left), expected);
            assert_eq!(Endpoint::classify(right), expected);
            assert!(expected.inference_query_allowed(""));
            assert_eq!(
                expected.inference_query_allowed("beta=true"),
                expected == Endpoint::Messages
            );
            assert!(!expected.inference_query_allowed("beta=true&beta=true"));
        }
    }

    #[test]
    fn opaque_history_ids_decode_once_without_reclassifying_content() {
        assert_eq!(
            Endpoint::classify("/responses/compact")
                .history_id()
                .unwrap()
                .as_deref(),
            Some("compact")
        );
        assert_eq!(
            Endpoint::classify("/responses/opaque%2B%2Fid%3D")
                .history_id()
                .unwrap()
                .as_deref(),
            Some("opaque+/id=")
        );
        assert_eq!(
            Endpoint::classify("/v1/responses/literal%252Fid")
                .history_id()
                .unwrap()
                .as_deref(),
            Some("literal%2Fid")
        );
        assert_eq!(
            Endpoint::classify("/responses/raw/slash")
                .history_id()
                .unwrap()
                .as_deref(),
            Some("raw/slash")
        );
        assert_eq!(
            Endpoint::classify("/responses/%63ompact")
                .history_id()
                .unwrap()
                .as_deref(),
            Some("compact")
        );
        assert!(Endpoint::classify("/responses/%FF").history_id().is_err());
        assert_eq!(
            Endpoint::classify("/responses/compact"),
            Endpoint::Compaction
        );
        assert_eq!(Endpoint::classify("/responses/"), Endpoint::Unknown);
    }
}
