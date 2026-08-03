//! The query that discovery sends.
//!
//! A resolver matches a stream with an XPath 1.0 predicate
//! (`src/stream_info_impl.cpp:214`). A program that builds that predicate with
//! string formatting has two faults. A value that holds an apostrophe breaks the
//! predicate. A value that comes from a peer can change what the predicate
//! selects. This type builds the predicate and escapes every value.

/// A discovery query.
///
/// Build one with a constructor. Join two with [`Query::and`] or [`Query::or`].
///
/// ```no_run
/// # use labstream::Query;
/// let q = Query::stream_type("EEG").and(Query::property("manufacturer", "Mentalab"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query(String);

impl Query {
    /// Every stream of this session.
    ///
    /// The session comes from the configuration. This is the query that liblsl
    /// sends for `lsl_resolve_all` (`src/lsl_resolver_c.cpp`).
    pub fn all() -> Query {
        Query(format!(
            "session_id={}",
            literal(&labstream_net::config::get().session_id)
        ))
    }

    /// The streams with this name.
    pub fn name(name: &str) -> Query {
        Query::property("name", name)
    }

    /// The streams with this content type, such as `EEG` or `Markers`.
    pub fn stream_type(stream_type: &str) -> Query {
        Query::property("type", stream_type)
    }

    /// The streams with this source identifier.
    ///
    /// A source identifier stays the same when the source restarts. Prefer it
    /// over the name to find one known stream again.
    pub fn source_id(source_id: &str) -> Query {
        Query::property("source_id", source_id)
    }

    /// The streams whose named property holds this value.
    ///
    /// The value is escaped. A value with an apostrophe is safe.
    pub fn property(key: &str, value: &str) -> Query {
        Query(format!("{key}={}", literal(value)))
    }

    /// Only the streams of this session.
    ///
    /// LSL groups the machines of one experiment with a session id. A stream of
    /// another session on the same network does not match. The id comes from
    /// `lab.SessionID` of the configuration file, and it is `default` when no
    /// file sets it.
    ///
    /// This is the query for a browser that lists the streams of the local
    /// experiment and not every stream on the subnet.
    pub fn session() -> Query {
        Query::property("session_id", &labstream_net::config::get().session_id)
    }

    /// A predicate that the caller writes.
    ///
    /// CAUTION: This text goes to the resolver as it is written. Do not build it
    /// from a value that another program supplies. Use [`Query::property`] for
    /// that value.
    pub fn predicate(xpath: &str) -> Query {
        Query(xpath.to_string())
    }

    /// The streams that match this query and the other query.
    pub fn and(self, other: Query) -> Query {
        Query(format!("({}) and ({})", self.0, other.0))
    }

    /// The streams that match this query, the other query, or both.
    pub fn or(self, other: Query) -> Query {
        Query(format!("({}) or ({})", self.0, other.0))
    }

    /// The predicate text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Write a value as an XPath 1.0 string literal.
///
/// XPath 1.0 has no escape character inside a literal. A value that holds both
/// quote characters therefore needs `concat()`. This function gives the shortest
/// form that is correct for the value.
fn literal(v: &str) -> String {
    if !v.contains('\'') {
        return format!("'{v}'");
    }
    if !v.contains('"') {
        return format!("\"{v}\"");
    }
    let parts = v
        .split('\'')
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(", \"'\", ");
    format!("concat({parts})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_value_uses_apostrophes() {
        assert_eq!(Query::name("EEG").as_str(), "name='EEG'");
    }

    #[test]
    fn a_value_with_an_apostrophe_uses_quotes() {
        assert_eq!(Query::name("Bob's EEG").as_str(), "name=\"Bob's EEG\"");
    }

    #[test]
    fn a_session_query_names_the_session_of_this_machine() {
        let q = Query::session();
        let want = &labstream_net::config::get().session_id;
        assert!(q.as_str().starts_with("session_id="), "{}", q.as_str());
        assert!(q.as_str().contains(want.as_str()), "{}", q.as_str());
    }

    #[test]
    fn a_value_with_both_quotes_uses_concat() {
        let q = Query::name("a'b\"c");
        assert!(q.as_str().starts_with("name=concat("), "{}", q.as_str());
    }
}
