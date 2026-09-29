use encoding_rs::WINDOWS_1252;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl TextEncoding {
    pub(crate) fn parse(raw: &str) -> Result<Self, &'static str> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "UTF-8" | "UTF8" => Ok(Self::Utf8),
            "UTF-16LE" | "UTF16LE" => Ok(Self::Utf16Le),
            "UTF-16BE" | "UTF16BE" => Ok(Self::Utf16Be),
            "WINDOWS-1252" | "CP1252" => Ok(Self::Windows1252),
            _ => Err("target_encoding must be UTF-8, UTF-16LE, UTF-16BE, or Windows-1252"),
        }
    }

    pub(crate) fn canonical_name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Windows1252 => "WINDOWS-1252",
        }
    }

    pub(crate) fn encode(self, content: &str) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Utf8 => Ok(content.as_bytes().to_vec()),
            Self::Utf16Le => Ok(encode_utf16_with_bom(content, true)),
            Self::Utf16Be => Ok(encode_utf16_with_bom(content, false)),
            Self::Windows1252 => {
                let (cow, _, has_unmappable) = WINDOWS_1252.encode(content);
                if has_unmappable {
                    return Err("content cannot be losslessly converted to Windows-1252");
                }
                Ok(cow.into_owned())
            }
        }
    }
}

fn encode_utf16_with_bom(content: &str, little_endian: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + content.len() * 2);
    bytes.extend_from_slice(if little_endian {
        &[0xFF, 0xFE]
    } else {
        &[0xFE, 0xFF]
    });
    for unit in content.encode_utf16() {
        let encoded = if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        bytes.extend_from_slice(&encoded);
    }
    bytes
}
