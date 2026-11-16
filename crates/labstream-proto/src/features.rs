//! The feature header between two labstream peers. `docs/extensions.md`.
//!
//! A labstream inlet offers features in the request header
//! `Labstream-Features`, and a labstream outlet answers the ones that it
//! accepts. liblsl reads only the header keys that it knows, on both sides
//! (`src/tcp_server.cpp:605-633`, `src/data_receiver.cpp:215-253`), so a
//! liblsl peer ignores the header and the connection stays plain LSL.
//!
//! Both parsers cut a header line at the first `;` and convert it to
//! lowercase (`src/tcp_server.cpp:610-613`, `crate::header`). A token is
//! therefore lowercase, and a parameter is hexadecimal.

/// The header name, as it goes on the wire.
pub const HEADER: &str = "Labstream-Features";

/// The header name, as [`crate::Headers`] stores it.
pub const HEADER_KEY: &str = "labstream-features";

/// The name of the sequence number feature. `crate::seq`.
pub const SEQ: &str = "seq";

/// The features that this library implements, each with its versions.
pub const SUPPORTED: &[(&str, &[u32])] = &[(SEQ, &[1])];

/// One feature with a version, and an optional parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The feature name: `a-z`, `0-9`, and `-`.
    pub name: String,
    /// The version of the feature.
    pub version: u32,
    /// A hexadecimal parameter, in lowercase.
    pub param: Option<String>,
}

impl Token {
    /// A token with no parameter.
    pub fn new(name: &str, version: u32) -> Self {
        Token {
            name: name.to_string(),
            version,
            param: None,
        }
    }

    /// Read one token. The result is `None` if the token does not match the
    /// syntax.
    pub fn parse(text: &str) -> Option<Token> {
        let (head, param) = match text.split_once('=') {
            Some((h, p)) => (h, Some(p)),
            None => (text, None),
        };
        let (name, version) = head.split_once('/')?;
        let name_ok = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        let version_ok = !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit());
        if !name_ok || !version_ok {
            return None;
        }
        let param = match param {
            Some(p) if !p.is_empty() && p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) => {
                Some(p.to_string())
            }
            Some(_) => return None,
            None => None,
        };
        Some(Token {
            name: name.to_string(),
            version: version.parse().ok()?,
            param,
        })
    }

    /// Write the token as it goes on the wire.
    pub fn to_wire(&self) -> String {
        match &self.param {
            Some(p) => format!("{}/{}={}", self.name, self.version, p),
            None => format!("{}/{}", self.name, self.version),
        }
    }
}

/// Read a header value. A token with a bad form is left out.
pub fn parse(value: &str) -> Vec<Token> {
    value.split_ascii_whitespace().filter_map(Token::parse).collect()
}

/// Write a whole header line, with its line end.
pub fn header_line(tokens: &[Token]) -> String {
    let list: Vec<String> = tokens.iter().map(Token::to_wire).collect();
    format!("{HEADER}: {}\r\n", list.join(" "))
}

/// The tokens that this process offers: each supported feature that the
/// configuration allows, at each supported version.
pub fn offer(allowed: &[String]) -> Vec<Token> {
    SUPPORTED
        .iter()
        .filter(|(name, _)| allowed.iter().any(|a| a == name))
        .flat_map(|(name, versions)| versions.iter().map(|v| Token::new(name, *v)))
        .collect()
}

/// The outlet's answer to an offer.
///
/// The answer holds only offered tokens. For each name it holds at most one
/// version: the highest version that both sides support. A feature that the
/// configuration does not allow is left out.
pub fn answer(offered: &[Token], allowed: &[String]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    for (name, versions) in SUPPORTED {
        if !allowed.iter().any(|a| a == name) {
            continue;
        }
        let best = offered
            .iter()
            .filter(|t| t.name == *name && versions.contains(&t.version) && t.param.is_none())
            .max_by_key(|t| t.version);
        if let Some(t) = best {
            out.push(t.clone());
        }
    }
    out
}

/// The features that are on, from what the inlet offered and what the outlet
/// answered.
///
/// An answer token that was not offered is left out. A feature is on only
/// when both sides named it.
pub fn agreed(offered: &[Token], answered: &[Token]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    for t in answered {
        let was_offered = offered
            .iter()
            .any(|o| o.name == t.name && o.version == t.version);
        if was_offered && !out.iter().any(|x| x.name == t.name) {
            out.push(t.clone());
        }
    }
    out
}

/// True when the list holds the feature at this version.
pub fn has(tokens: &[Token], name: &str, version: u32) -> bool {
    tokens.iter().any(|t| t.name == name && t.version == version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<String> {
        vec![SEQ.to_string()]
    }

    #[test]
    fn a_token_reads_back_as_it_was_written() {
        for text in ["seq/1", "shm/1=0a1b2c", "x-y2/10"] {
            assert_eq!(Token::parse(text).unwrap().to_wire(), text);
        }
    }

    #[test]
    fn a_bad_token_is_left_out() {
        let got = parse("seq/1 SEQ/1 seq/ /1 seq/x shm/1=XYZ shm/1= seq/2");
        assert_eq!(got, vec![Token::new("seq", 1), Token::new("seq", 2)]);
    }

    #[test]
    fn the_header_line_is_lowercase_safe_and_has_no_semicolon() {
        let line = header_line(&[Token::new("seq", 1)]);
        assert_eq!(line, "Labstream-Features: seq/1\r\n");
        // Both parsers cut the line at `;` and convert it to lowercase. The
        // value must read the same after that.
        let value = line.split_once(':').unwrap().1.trim();
        assert!(!value.contains(';'));
        assert_eq!(parse(&value.to_ascii_lowercase()), parse(value));
    }

    #[test]
    fn the_answer_holds_only_offered_and_allowed_features() {
        let offered = parse("seq/1 seq/7 shm/1=00ff");
        assert_eq!(answer(&offered, &all()), vec![Token::new("seq", 1)]);
        assert!(answer(&offered, &[]).is_empty());
        assert!(answer(&parse("shm/1"), &all()).is_empty());
        assert!(answer(&parse("seq/9"), &all()).is_empty());
    }

    #[test]
    fn a_feature_is_on_only_when_both_sides_name_it() {
        let offered = offer(&all());
        assert_eq!(offered, vec![Token::new("seq", 1)]);
        assert!(has(&agreed(&offered, &parse("seq/1")), SEQ, 1));
        assert!(agreed(&offered, &parse("seq/2")).is_empty());
        assert!(agreed(&offered, &[]).is_empty());
        assert!(agreed(&[], &parse("seq/1")).is_empty());
        assert_eq!(agreed(&offered, &parse("seq/1 seq/1")).len(), 1);
    }

    #[test]
    fn the_configuration_can_turn_every_feature_off() {
        assert!(offer(&[]).is_empty());
    }
}
