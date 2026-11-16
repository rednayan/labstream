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

/// The name of the resend feature. `docs/extensions.md`, section `resume`.
///
/// `resume` is not in [`SUPPORTED`], because its tokens carry a parameter.
/// [`offer_resume`], [`resume_request`], and [`resume_answer`] handle it.
pub const RESUME: &str = "resume";

/// The features with no parameter that this library implements, each with
/// its versions.
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
            Some(p)
                if !p.is_empty() && p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
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
    value
        .split_ascii_whitespace()
        .filter_map(Token::parse)
        .collect()
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
/// configuration does not allow is left out. An offered token with a
/// parameter is left out. The answer for `resume` comes from
/// [`resume_request`].
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
/// when both sides named it. The comparison uses the name and the version.
/// The result keeps the parameter of the answer token.
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
    tokens
        .iter()
        .any(|t| t.name == name && t.version == version)
}

/// The count of hexadecimal digits of the epoch in a `resume/1` parameter.
const EPOCH_DIGITS: usize = 16;

/// The parameter of a `resume/1` token.
///
/// The parameter is the epoch in exactly 16 hexadecimal digits, then a
/// sample number in 1 to 16 hexadecimal digits, with no separator. In an
/// offer, the number is the next number that the inlet expects. In an
/// answer, the number is the first live sample of the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeParam {
    /// The random value that identifies one run of the outlet.
    pub epoch: u64,
    /// A `seq` number.
    pub number: u64,
}

impl ResumeParam {
    /// Read a parameter. The result is `None` if the parameter has fewer
    /// than 17 or more than 32 digits, or a digit that is not hexadecimal.
    pub fn parse(param: &str) -> Option<ResumeParam> {
        if param.len() <= EPOCH_DIGITS || param.len() > 2 * EPOCH_DIGITS {
            return None;
        }
        // `from_str_radix` also accepts a sign, so each byte gets a check.
        if !param
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        let (epoch, number) = param.split_at(EPOCH_DIGITS);
        Some(ResumeParam {
            epoch: u64::from_str_radix(epoch, 16).ok()?,
            number: u64::from_str_radix(number, 16).ok()?,
        })
    }

    /// Write the parameter as it goes on the wire.
    pub fn to_param(&self) -> String {
        format!("{:016x}{:x}", self.epoch, self.number)
    }

    /// A `resume/1` token with this parameter.
    pub fn token(&self) -> Token {
        Token {
            name: RESUME.to_string(),
            version: 1,
            param: Some(self.to_param()),
        }
    }
}

/// What an inlet asks for with its `resume/1` token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRequest {
    /// A token with no parameter: a first connection, or a new outlet.
    Fresh,
    /// A token with a parameter: a recovery to the same outlet.
    From(ResumeParam),
}

/// True when the configuration allows `resume`. `resume` needs `seq`, so the
/// configuration must allow both.
fn resume_allowed(allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == SEQ) && allowed.iter().any(|a| a == RESUME)
}

/// The `resume/1` token that an inlet offers.
///
/// `from` is `None` on a first connection, after a recovery to a new outlet,
/// and when the inlet has no epoch or no expected number. The result is
/// `None` when the configuration does not allow `resume`.
pub fn offer_resume(allowed: &[String], from: Option<ResumeParam>) -> Option<Token> {
    if !resume_allowed(allowed) {
        return None;
    }
    Some(match from {
        Some(p) => p.token(),
        None => Token::new(RESUME, 1),
    })
}

/// Read the `resume/1` request of an offer, on the outlet side.
///
/// `answered` is the result of [`answer`] for the same offer. The result is
/// `None` when `resume` must stay off for the connection:
///
/// - the configuration does not allow `resume`,
/// - the answer does not hold `seq/1`,
/// - the offer holds no `resume/1` token, or more than one,
/// - the parameter does not match [`ResumeParam::parse`].
///
/// If the result is not `None`, the outlet adds the token of its own
/// [`ResumeParam`] to the answer.
pub fn resume_request(
    offered: &[Token],
    answered: &[Token],
    allowed: &[String],
) -> Option<ResumeRequest> {
    if !resume_allowed(allowed) || !has(answered, SEQ, 1) {
        return None;
    }
    let mut tokens = offered
        .iter()
        .filter(|t| t.name == RESUME && t.version == 1);
    let token = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    match &token.param {
        None => Some(ResumeRequest::Fresh),
        Some(p) => ResumeParam::parse(p).map(ResumeRequest::From),
    }
}

/// Read the `resume/1` answer of an outlet, on the inlet side.
///
/// `agreed` is the result of [`agreed`]. The result is `None` when `resume`
/// is off for the connection: the features do not hold `seq/1`, or do not
/// hold `resume/1`, or the parameter does not match [`ResumeParam::parse`].
pub fn resume_answer(agreed: &[Token]) -> Option<ResumeParam> {
    if !has(agreed, SEQ, 1) {
        return None;
    }
    let token = agreed.iter().find(|t| t.name == RESUME && t.version == 1)?;
    ResumeParam::parse(token.param.as_deref()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<String> {
        vec![SEQ.to_string()]
    }

    fn with_resume() -> Vec<String> {
        vec![SEQ.to_string(), RESUME.to_string()]
    }

    const P: ResumeParam = ResumeParam {
        epoch: 0x9c3e_51a0_d2b7_f4e8,
        number: 0x1f4a0,
    };

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

    #[test]
    fn a_resume_parameter_reads_back_as_it_was_written() {
        assert_eq!(P.to_param(), "9c3e51a0d2b7f4e81f4a0");
        assert_eq!(ResumeParam::parse("9c3e51a0d2b7f4e81f4a0"), Some(P));
        // A small epoch keeps its leading zeros, so the split stays at 16.
        let small = ResumeParam {
            epoch: 1,
            number: 0,
        };
        assert_eq!(small.to_param(), "00000000000000010");
        assert_eq!(ResumeParam::parse(&small.to_param()), Some(small));
        let max = ResumeParam {
            epoch: u64::MAX,
            number: u64::MAX,
        };
        assert_eq!(max.to_param().len(), 32);
        assert_eq!(ResumeParam::parse(&max.to_param()), Some(max));
    }

    #[test]
    fn a_resume_parameter_with_a_bad_length_or_digit_is_refused() {
        // 16 digits: an epoch and no number.
        assert_eq!(ResumeParam::parse("9c3e51a0d2b7f4e8"), None);
        // 33 digits: the number does not fit in 64 bits.
        assert_eq!(ResumeParam::parse(&"1".repeat(33)), None);
        assert_eq!(ResumeParam::parse(""), None);
        assert_eq!(ResumeParam::parse("9c3e51a0d2b7f4e8+1"), None);
        assert_eq!(ResumeParam::parse("9C3E51A0D2B7F4E81"), None);
    }

    #[test]
    fn the_resume_token_goes_through_the_header_syntax() {
        let line = header_line(&[Token::new(SEQ, 1), P.token()]);
        let value = line.split_once(':').unwrap().1.trim();
        assert_eq!(value, "seq/1 resume/1=9c3e51a0d2b7f4e81f4a0");
        assert_eq!(parse(&value.to_ascii_lowercase()), parse(value));
        assert_eq!(parse(value)[1], P.token());
    }

    #[test]
    fn an_inlet_offers_resume_only_with_seq() {
        assert_eq!(
            offer_resume(&with_resume(), None),
            Some(Token::new(RESUME, 1))
        );
        assert_eq!(offer_resume(&with_resume(), Some(P)), Some(P.token()));
        assert_eq!(offer_resume(&all(), Some(P)), None);
        assert_eq!(offer_resume(&[RESUME.to_string()], None), None);
        assert_eq!(offer_resume(&[], None), None);
    }

    #[test]
    fn the_plain_answer_leaves_resume_out() {
        let offered = parse("seq/1 resume/1 resume/1=9c3e51a0d2b7f4e81f4a0");
        assert_eq!(answer(&offered, &with_resume()), vec![Token::new(SEQ, 1)]);
    }

    #[test]
    fn an_outlet_reads_the_resume_request() {
        let req = |text: &str, allowed: &[String]| {
            let offered = parse(text);
            let answered = answer(&offered, allowed);
            resume_request(&offered, &answered, allowed)
        };
        let a = with_resume();
        assert_eq!(req("seq/1 resume/1", &a), Some(ResumeRequest::Fresh));
        assert_eq!(
            req("seq/1 resume/1=9c3e51a0d2b7f4e81f4a0", &a),
            Some(ResumeRequest::From(P))
        );
        // Another version of resume is not this request.
        assert_eq!(req("seq/1 resume/2", &a), None);
        assert_eq!(
            req("seq/1 resume/2 resume/1", &a),
            Some(ResumeRequest::Fresh)
        );
    }

    #[test]
    fn an_outlet_keeps_resume_off_for_a_bad_request() {
        let req = |text: &str, allowed: &[String]| {
            let offered = parse(text);
            let answered = answer(&offered, allowed);
            resume_request(&offered, &answered, allowed)
        };
        let a = with_resume();
        // Without seq on the connection.
        assert_eq!(req("resume/1", &a), None);
        assert_eq!(req("seq/1 resume/1", &[RESUME.to_string()]), None);
        // Not allowed by the configuration.
        assert_eq!(req("seq/1 resume/1", &all()), None);
        // A parameter of 16 or 33 digits.
        assert_eq!(req("seq/1 resume/1=9c3e51a0d2b7f4e8", &a), None);
        let long = format!("seq/1 resume/1={}", "1".repeat(33));
        assert_eq!(req(&long, &a), None);
        // Two resume/1 tokens, with the same or a different parameter.
        assert_eq!(req("seq/1 resume/1 resume/1", &a), None);
        assert_eq!(
            req(
                "seq/1 resume/1=9c3e51a0d2b7f4e81f4a0 resume/1=9c3e51a0d2b7f4e82",
                &a
            ),
            None
        );
        // No resume token.
        assert_eq!(req("seq/1", &a), None);
    }

    #[test]
    fn an_inlet_reads_the_resume_answer() {
        let offered = parse("seq/1 resume/1");
        let on = agreed(&offered, &[Token::new(SEQ, 1), P.token()]);
        assert_eq!(resume_answer(&on), Some(P));
        // The answer has no parameter.
        let bare = agreed(&offered, &parse("seq/1 resume/1"));
        assert_eq!(resume_answer(&bare), None);
        // The answer has a bad parameter.
        let bad = agreed(&offered, &parse("seq/1 resume/1=9c3e51a0d2b7f4e8"));
        assert_eq!(resume_answer(&bad), None);
        // The answer has resume and no seq.
        let lone = agreed(&offered, &[P.token()]);
        assert_eq!(resume_answer(&lone), None);
        // The inlet did not offer resume.
        let unasked = agreed(&parse("seq/1"), &[Token::new(SEQ, 1), P.token()]);
        assert_eq!(resume_answer(&unasked), None);
        assert!(has(&unasked, SEQ, 1));
    }
}
