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

/// The name of the UDP transport. `docs/extensions.md`, section `dgram`.
///
/// `dgram` is not in [`SUPPORTED`], because its answer carries a parameter.
/// [`offer_dgram`], [`dgram_request`], and [`dgram_answer`] handle it. It is
/// off by default in version 1.
pub const DGRAM: &str = "dgram";

/// The name of the channel and rate selection feature (`docs/roadmap.md`, step 6).
///
/// `select` carries a hexadecimal parameter naming the decimation factor and
/// optional channel index list. [`offer_select`], [`select_request`], and
/// [`select_answer`] handle it.
pub const SELECT: &str = "select";

/// The name of the same-machine shared-memory transport (`docs/extensions.md`,
/// section `shm`, `docs/roadmap.md`, step 9).
///
/// `shm` carries a 16-hex-digit segment identifier in the outlet answer.
/// [`offer_shm`], [`shm_request`], and [`shm_answer`] handle it.
pub const SHM: &str = "shm";

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

/// The parameter of a `dgram/1` answer: the UDP port of the outlet in
/// exactly 4 hexadecimal digits, then a cookie in exactly 16 hexadecimal
/// digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DgramParam {
    /// The UDP port of the outlet.
    pub port: u16,
    /// The random value that each datagram carries.
    pub cookie: u64,
}

impl DgramParam {
    /// Read a parameter. The result is `None` unless the parameter has
    /// exactly 20 hexadecimal digits.
    pub fn parse(param: &str) -> Option<DgramParam> {
        if param.len() != 20
            || !param
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        Some(DgramParam {
            port: u16::from_str_radix(&param[..4], 16).ok()?,
            cookie: u64::from_str_radix(&param[4..], 16).ok()?,
        })
    }

    /// Write the parameter as it goes on the wire.
    pub fn to_param(&self) -> String {
        format!("{:04x}{:016x}", self.port, self.cookie)
    }

    /// A `dgram/1` token with this parameter.
    pub fn token(&self) -> Token {
        Token {
            name: DGRAM.to_string(),
            version: 1,
            param: Some(self.to_param()),
        }
    }
}

/// True when the configuration allows `dgram`. `dgram` needs `seq` and
/// `resume`, so the configuration must allow all three.
fn dgram_allowed(allowed: &[String]) -> bool {
    resume_allowed(allowed) && allowed.iter().any(|a| a == DGRAM)
}

/// The `dgram/1` token that an inlet offers, or `None` when the
/// configuration does not allow `dgram`.
pub fn offer_dgram(allowed: &[String]) -> Option<Token> {
    dgram_allowed(allowed).then(|| Token::new(DGRAM, 1))
}

/// True when the outlet can answer `dgram/1`.
///
/// `answered` must already hold the answer tokens of `seq/1` and of
/// `resume/1`. The offer must hold `dgram/1` with no parameter.
/// `docs/extensions.md`, section `dgram`, negotiation rules 2 and 6.
pub fn dgram_request(offered: &[Token], answered: &[Token], allowed: &[String]) -> bool {
    dgram_allowed(allowed)
        && has(answered, SEQ, 1)
        && has(answered, RESUME, 1)
        && offered
            .iter()
            .any(|t| t.name == DGRAM && t.version == 1 && t.param.is_none())
}

/// Read the `dgram/1` answer of an outlet, on the inlet side.
///
/// The result is `None` when `dgram` is off for the connection: the
/// features do not hold `seq/1` and `resume/1`, or do not hold `dgram/1`,
/// or the parameter does not match [`DgramParam::parse`].
pub fn dgram_answer(agreed: &[Token]) -> Option<DgramParam> {
    resume_answer(agreed)?;
    let token = agreed.iter().find(|t| t.name == DGRAM && t.version == 1)?;
    DgramParam::parse(token.param.as_deref()?)
}

/// The parameter of a `select/1` offer and answer (`docs/roadmap.md`, step 6).
///
/// Wire encoding (lowercase hex, no separator):
/// - 8 hex digits: `decimate` as `u32` big-endian (`>= 1`; `1` = full rate)
/// - 4 hex digits: `channels.len()` as `u16` big-endian (`0` = all channels)
/// - `4 * channels.len()` hex digits: each 0-based channel index as `u16` big-endian
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectParam {
    /// Integer rate decimation factor (`1` keeps the full rate, `16` reduces
    /// 4000 Hz to 250 Hz).
    pub decimate: u32,
    /// Ordered 0-based channel indices to keep. Empty means every channel.
    pub channels: Vec<u16>,
}

impl SelectParam {
    /// Select a subset of channels at full rate.
    pub fn channels(channels: Vec<u16>) -> Self {
        SelectParam {
            decimate: 1,
            channels,
        }
    }

    /// Decimate all channels by `factor` (clamped to `>= 1`).
    pub fn decimate(factor: u32) -> Self {
        SelectParam {
            decimate: factor.max(1),
            channels: Vec::new(),
        }
    }

    /// True when this parameter actually narrows channels or reduces rate.
    pub fn is_active(&self) -> bool {
        self.decimate > 1 || !self.channels.is_empty()
    }

    /// Effective channel count for a source stream with `source_channels`.
    pub fn output_channels(&self, source_channels: usize) -> usize {
        if self.channels.is_empty() {
            source_channels
        } else {
            self.channels.len()
        }
    }

    /// Effective sample rate for a source stream with `source_srate`.
    pub fn output_srate(&self, source_srate: f64) -> f64 {
        if source_srate <= 0.0 || self.decimate <= 1 {
            source_srate
        } else {
            source_srate / self.decimate as f64
        }
    }

    /// Read a `select/1` parameter from its lowercase hexadecimal string.
    pub fn parse(param: &str) -> Option<SelectParam> {
        if param.len() < 12
            || (param.len() - 12) % 4 != 0
            || !param
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        let decimate = u32::from_str_radix(&param[..8], 16).ok()?;
        if decimate == 0 {
            return None;
        }
        let count = u16::from_str_radix(&param[8..12], 16).ok()? as usize;
        if param.len() != 12 + count * 4 {
            return None;
        }
        let mut channels = Vec::with_capacity(count);
        for i in 0..count {
            let start = 12 + i * 4;
            channels.push(u16::from_str_radix(&param[start..start + 4], 16).ok()?);
        }
        Some(SelectParam { decimate, channels })
    }

    /// Encode this parameter as lowercase hexadecimal.
    pub fn to_param(&self) -> String {
        let mut s = format!(
            "{:08x}{:04x}",
            self.decimate.max(1),
            self.channels.len() as u16
        );
        for &ch in &self.channels {
            s.push_str(&format!("{ch:04x}"));
        }
        s
    }

    /// Build the `select/1=<hex>` wire token.
    pub fn token(&self) -> Token {
        Token {
            name: SELECT.to_string(),
            version: 1,
            param: Some(self.to_param()),
        }
    }
}

/// True when the configuration allows `select`.
fn select_allowed(allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == SELECT)
}

/// Build the `select/1` offer token when selection is active and allowed.
pub fn offer_select(allowed: &[String], param: Option<&SelectParam>) -> Option<Token> {
    if !select_allowed(allowed) {
        return None;
    }
    let p = param?;
    if !p.is_active() || p.channels.len() > u16::MAX as usize {
        return None;
    }
    Some(p.token())
}

/// Validate an inlet's `select/1` offer against the outlet's stream metadata.
///
/// Rejects out-of-range channel indices, decimation on irregular or string
/// streams, duplicate tokens, or inactive requests.
pub fn select_request(
    offered: &[Token],
    allowed: &[String],
    source_channels: usize,
    source_srate: f64,
    is_string: bool,
) -> Option<SelectParam> {
    if !select_allowed(allowed) {
        return None;
    }
    let mut tokens = offered
        .iter()
        .filter(|t| t.name == SELECT && t.version == 1);
    let token = tokens.next()?;
    if tokens.next().is_some() {
        return None;
    }
    let param = SelectParam::parse(token.param.as_deref()?)?;
    if !param.is_active() {
        return None;
    }
    if param.decimate > 1 && (is_string || source_srate <= 0.0 || !source_srate.is_finite()) {
        return None;
    }
    if param
        .channels
        .iter()
        .any(|&ch| (ch as usize) >= source_channels)
    {
        return None;
    }
    Some(param)
}

/// Read the outlet's confirmed `select/1` answer on the inlet side.
pub fn select_answer(agreed: &[Token]) -> Option<SelectParam> {
    let token = agreed.iter().find(|t| t.name == SELECT && t.version == 1)?;
    let param = SelectParam::parse(token.param.as_deref()?)?;
    param.is_active().then_some(param)
}

/// The parameter of a `shm/1` answer: a 64-bit random segment identifier
/// encoded as exactly 16 lowercase hexadecimal digits (`docs/extensions.md`,
/// section `shm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmParam {
    /// Random 64-bit identifier naming the shared-memory segment.
    pub id: u64,
}

impl ShmParam {
    /// Read a `shm/1` parameter. Returns `None` unless the string holds
    /// exactly 16 lowercase hexadecimal digits.
    pub fn parse(param: &str) -> Option<ShmParam> {
        if param.len() != 16
            || !param
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        Some(ShmParam {
            id: u64::from_str_radix(param, 16).ok()?,
        })
    }

    /// Encode this parameter as 16 lowercase hexadecimal digits.
    pub fn to_param(&self) -> String {
        format!("{:016x}", self.id)
    }

    /// Build the `shm/1=<id>` wire token.
    pub fn token(&self) -> Token {
        Token {
            name: SHM.to_string(),
            version: 1,
            param: Some(self.to_param()),
        }
    }
}

/// True when the configuration allows `shm`.
fn shm_allowed(allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == SHM)
}

/// The `shm/1` token that an inlet offers, or `None` when `shm` is disabled.
pub fn offer_shm(allowed: &[String]) -> Option<Token> {
    shm_allowed(allowed).then(|| Token::new(SHM, 1))
}

/// True when the outlet can answer `shm/1` for this request.
pub fn shm_request(offered: &[Token], allowed: &[String]) -> bool {
    shm_allowed(allowed)
        && offered
            .iter()
            .any(|t| t.name == SHM && t.version == 1 && t.param.is_none())
}

/// Read the outlet's `shm/1=<id>` answer on the inlet side.
pub fn shm_answer(agreed: &[Token]) -> Option<ShmParam> {
    let token = agreed.iter().find(|t| t.name == SHM && t.version == 1)?;
    ShmParam::parse(token.param.as_deref()?)
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

    fn with_dgram() -> Vec<String> {
        vec![SEQ.to_string(), RESUME.to_string(), DGRAM.to_string()]
    }

    const G: DgramParam = DgramParam {
        port: 0xc35b,
        cookie: 0x9f2e_81d0_4a77_13c6,
    };

    #[test]
    fn a_dgram_parameter_reads_back_as_it_was_written() {
        assert_eq!(G.to_param(), "c35b9f2e81d04a7713c6");
        assert_eq!(DgramParam::parse("c35b9f2e81d04a7713c6"), Some(G));
        let small = DgramParam { port: 1, cookie: 2 };
        assert_eq!(small.to_param().len(), 20);
        assert_eq!(DgramParam::parse(&small.to_param()), Some(small));
        assert_eq!(DgramParam::parse("c35b9f2e81d04a7713c"), None);
        assert_eq!(DgramParam::parse("c35b9f2e81d04a7713c60"), None);
        assert_eq!(DgramParam::parse("+35b9f2e81d04a7713c6"), None);
    }

    #[test]
    fn an_inlet_offers_dgram_only_with_seq_and_resume() {
        assert_eq!(offer_dgram(&with_dgram()), Some(Token::new(DGRAM, 1)));
        assert_eq!(offer_dgram(&with_resume()), None);
        assert_eq!(offer_dgram(&[SEQ.to_string(), DGRAM.to_string()]), None);
        assert_eq!(offer_dgram(&[DGRAM.to_string()]), None);
    }

    #[test]
    fn an_outlet_answers_dgram_only_with_seq_and_resume_in_the_answer() {
        let offered = parse("seq/1 resume/1 dgram/1");
        let full = vec![Token::new(SEQ, 1), P.token()];
        assert!(dgram_request(&offered, &full, &with_dgram()));
        // Not allowed here.
        assert!(!dgram_request(&offered, &full, &with_resume()));
        // No resume in the answer.
        assert!(!dgram_request(
            &offered,
            &[Token::new(SEQ, 1)],
            &with_dgram()
        ));
        // No offer, or an offer with a parameter.
        assert!(!dgram_request(
            &parse("seq/1 resume/1"),
            &full,
            &with_dgram()
        ));
        assert!(!dgram_request(
            &parse("seq/1 resume/1 dgram/1=00"),
            &full,
            &with_dgram()
        ));
        // The plain answer leaves dgram out.
        assert_eq!(answer(&offered, &with_dgram()), vec![Token::new(SEQ, 1)]);
    }

    #[test]
    fn an_inlet_reads_the_dgram_answer() {
        let offered = parse("seq/1 resume/1 dgram/1");
        let on = agreed(&offered, &[Token::new(SEQ, 1), P.token(), G.token()]);
        assert_eq!(dgram_answer(&on), Some(G));
        // No resume on the connection.
        let lone = agreed(&offered, &[Token::new(SEQ, 1), G.token()]);
        assert_eq!(dgram_answer(&lone), None);
        // No parameter, or a bad one.
        let bare = agreed(
            &offered,
            &parse("seq/1 resume/1=9c3e51a0d2b7f4e81f4a0 dgram/1"),
        );
        assert_eq!(dgram_answer(&bare), None);
        let bad = agreed(
            &offered,
            &parse("seq/1 resume/1=9c3e51a0d2b7f4e81f4a0 dgram/1=00ff"),
        );
        assert_eq!(dgram_answer(&bad), None);
        // The inlet did not offer dgram.
        let unasked = agreed(
            &parse("seq/1 resume/1"),
            &[Token::new(SEQ, 1), P.token(), G.token()],
        );
        assert_eq!(dgram_answer(&unasked), None);
    }

    #[test]
    fn shm_token_round_trips_and_negotiates() {
        let p = ShmParam {
            id: 0x00ab_cd12_34ef_5678,
        };
        assert_eq!(p.to_param(), "00abcd1234ef5678");
        assert_eq!(ShmParam::parse(&p.to_param()), Some(p));
        assert_eq!(ShmParam::parse("00abcd1234ef567"), None);
        assert_eq!(ShmParam::parse("00ABCD1234EF5678"), None);

        let allowed = vec![SEQ.to_string(), SHM.to_string()];
        assert_eq!(offer_shm(&allowed), Some(Token::new(SHM, 1)));
        assert_eq!(offer_shm(&all()), None);

        let offered = parse("seq/1 shm/1");
        assert!(shm_request(&offered, &allowed));
        assert!(!shm_request(&offered, &all()));
        assert!(!shm_request(
            &parse("seq/1 shm/1=00abcd1234ef5678"),
            &allowed
        ));

        let on = agreed(&offered, &[Token::new(SEQ, 1), p.token()]);
        assert_eq!(shm_answer(&on), Some(p));
        assert_eq!(shm_answer(&agreed(&parse("seq/1"), &[p.token()])), None);
    }
}
