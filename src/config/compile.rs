//! One HTTP route, checked and compiled. Standalone `validate()`, the
//! Kubernetes route acceptance and the route-table build all call
//! [`compile_http_route`]: every path judges a route by the same rules, and a
//! table build compiles each regex once, however many hostnames and
//! listeners the route attaches to.

use std::fmt;

use super::types::*;
use crate::routing;

/// Accepted=False reasons of Gateway API v1.4.1 (RouteConditionReason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// A value this proxy cannot apply.
    UnsupportedValue,
    /// Filters that cannot share one rule.
    IncompatibleFilters,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedValue => "UnsupportedValue",
            Self::IncompatibleFilters => "IncompatibleFilters",
        }
    }

    /// The reason a route failed conversion or its check. An error without
    /// one names a value the proxy cannot apply.
    pub fn of(e: &anyhow::Error) -> Self {
        e.downcast_ref::<RouteError>()
            .map_or(Self::UnsupportedValue, |r| r.reason)
    }
}

/// Why a route is rejected: the spec reason, and the field at fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteError {
    pub reason: RejectReason,
    pub message: String,
}

impl RouteError {
    pub fn unsupported(message: impl fmt::Display) -> Self {
        Self {
            reason: RejectReason::UnsupportedValue,
            message: message.to_string(),
        }
    }

    pub fn incompatible(message: impl fmt::Display) -> Self {
        Self {
            reason: RejectReason::IncompatibleFilters,
            message: message.to_string(),
        }
    }

    /// Prefix the message with where in the route the error sits.
    pub fn at(mut self, place: impl fmt::Display) -> Self {
        self.message = format!("{}: {}", place, self.message);
        self
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RouteError {}

/// A route in the table's form, but for what DNS decides at build time.
pub(crate) struct CompiledRoute<'a> {
    pub rules: Vec<CompiledRule<'a>>,
}

pub(crate) struct CompiledRule<'a> {
    pub source: &'a HttpRouteRule,
    pub matches: Vec<CompiledMatch>,
    pub filters: Vec<CompiledFilter<'a>>,
    /// Filters of each entry of `source.backend_refs`, in order.
    pub backend_filters: Vec<Vec<routing::HttpFilter>>,
}

pub(crate) enum CompiledFilter<'a> {
    Ready(routing::HttpFilter),
    /// Its address is DNS's to decide, at table build.
    Mirror {
        backend: &'a BackendRef,
        fraction: routing::Fraction,
    },
}

pub(crate) struct CompiledMatch {
    method: Option<String>,
    path_match_type: routing::PathMatchType,
    path: String,
    path_regex: Option<regex::Regex>,
    headers: Vec<routing::HeaderMatch>,
    query_params: Vec<routing::QueryParamMatch>,
}

impl CompiledMatch {
    /// The table rule for this match. Clones share the compiled regexes.
    pub(crate) fn to_rule(
        &self,
        filters: &[routing::HttpFilter],
        backends: &[routing::Backend],
        timeouts: Option<&HttpRouteTimeouts>,
    ) -> routing::HttpRouteRule {
        let mut rule = routing::HttpRouteRule::new(
            self.path_match_type.clone(),
            self.path.clone(),
            self.headers.clone(),
            self.query_params.clone(),
            filters.to_vec(),
            backends.to_vec(),
        )
        .with_method(self.method.clone());
        rule.path_regex = self.path_regex.clone();
        if let Some(timeouts) = timeouts {
            rule.request_timeout = timeouts.request;
            rule.backend_request_timeout = timeouts.backend_request;
        }
        rule
    }
}

/// Check every rule of `route` and compile its matches and filters. The
/// first failure rejects the whole route.
pub(crate) fn compile_http_route(route: &HttpRouteConfig) -> Result<CompiledRoute<'_>, RouteError> {
    let rules = route
        .rules
        .iter()
        .enumerate()
        .map(|(i, rule)| compile_rule(rule).map_err(|e| e.at(format_args!("rule {}", i))))
        .collect::<Result<_, _>>()?;
    Ok(CompiledRoute { rules })
}

fn compile_rule(rule: &HttpRouteRule) -> Result<CompiledRule<'_>, RouteError> {
    check_filter_combination(&rule.filters)?;
    check_prefix_replacement(rule)?;

    let backend_filters = rule
        .backend_refs
        .iter()
        .enumerate()
        .map(|(i, b)| {
            compile_backend_filters(b).map_err(|e| e.at(format_args!("backendRef {}", i)))
        })
        .collect::<Result<_, _>>()?;
    // Gateway API v1.4.1 HTTPRouteRule.matches: "If no matches are
    // specified, the default is a prefix path match on "/"".
    let default_match = [HttpRouteMatch::path_prefix("/")];
    let matches = if rule.matches.is_empty() {
        &default_match[..]
    } else {
        &rule.matches[..]
    };
    let matches = matches
        .iter()
        .enumerate()
        .map(|(i, m)| compile_match(m).map_err(|e| e.at(format_args!("match {}", i))))
        .collect::<Result<_, _>>()?;
    let filters = rule
        .filters
        .iter()
        .enumerate()
        .map(|(i, f)| compile_filter(f).map_err(|e| e.at(format_args!("filter {}", i))))
        .collect::<Result<_, _>>()?;

    Ok(CompiledRule {
        source: rule,
        matches,
        filters,
        backend_filters,
    })
}

/// Combinations within one rule that would put a malformed message on the
/// wire, or that the spec forbids.
fn check_filter_combination(filters: &[HttpRouteFilter]) -> Result<(), RouteError> {
    check_no_repeats(filters)?;
    let has = |pred: fn(&HttpRouteFilter) -> bool| filters.iter().any(pred);
    if has(|f| matches!(f, HttpRouteFilter::RequestRedirect { .. }))
        && has(|f| matches!(f, HttpRouteFilter::URLRewrite { .. }))
    {
        return Err(RouteError::incompatible(
            "URLRewrite and RequestRedirect are mutually exclusive",
        ));
    }
    super::wire::check_rule_filters(filters).map_err(RouteError::incompatible)
}

/// Gateway API lets only RequestMirror repeat in one filter list. The data
/// plane applies the first filter of any other type and skips the rest.
fn check_no_repeats(filters: &[HttpRouteFilter]) -> Result<(), RouteError> {
    use std::mem::discriminant;

    for (i, f) in filters.iter().enumerate() {
        let repeated = !matches!(f, HttpRouteFilter::RequestMirror { .. })
            && filters[..i]
                .iter()
                .any(|earlier| discriminant(earlier) == discriminant(f));
        if repeated {
            return Err(RouteError::incompatible(format!(
                "filter {}: {} filter cannot be repeated",
                i,
                filter_type(f)
            )));
        }
    }
    Ok(())
}

fn filter_type(f: &HttpRouteFilter) -> &'static str {
    match f {
        HttpRouteFilter::RequestHeaderModifier { .. } => "RequestHeaderModifier",
        HttpRouteFilter::ResponseHeaderModifier { .. } => "ResponseHeaderModifier",
        HttpRouteFilter::RequestRedirect { .. } => "RequestRedirect",
        HttpRouteFilter::URLRewrite { .. } => "URLRewrite",
        HttpRouteFilter::RequestMirror { .. } => "RequestMirror",
    }
}

/// The data plane splices the request path by the matched prefix: Gateway
/// API allows ReplacePrefixMatch on PathPrefix matches only.
fn check_prefix_replacement(rule: &HttpRouteRule) -> Result<(), RouteError> {
    let replaces_prefix = rule.filters.iter().any(|f| {
        let path = match f {
            HttpRouteFilter::RequestRedirect { config } => config.path.as_ref(),
            HttpRouteFilter::URLRewrite { config } => config.path.as_ref(),
            _ => None,
        };
        matches!(path, Some(HttpURLRewritePath::ReplacePrefixMatch { .. }))
    });
    if !replaces_prefix {
        return Ok(());
    }
    for (i, m) in rule.matches.iter().enumerate() {
        if let Some(p) = &m.path {
            if p.match_type != HttpPathMatchType::PathPrefix {
                return Err(RouteError::incompatible(format!(
                    "match {}: ReplacePrefixMatch requires a PathPrefix path match, got {:?}",
                    i, p.match_type
                )));
            }
        }
    }
    Ok(())
}

fn compile_match(m: &HttpRouteMatch) -> Result<CompiledMatch, RouteError> {
    if let Some(method) = &m.method {
        // Same alphabet as the wire sniff: a whitelist here would reject
        // routes for the WebDAV methods the data plane forwards.
        if !crate::proxy::method::is_valid_method(method) {
            return Err(RouteError::unsupported(format!(
                "Invalid HTTP method '{}': expected {} to {} letters or '-' as IANA registers them (GET, PROPFIND, MKCALENDAR, ...)",
                method,
                crate::proxy::method::MIN_METHOD_LEN,
                crate::proxy::method::MAX_METHOD_LEN
            )));
        }
    }

    let (path_match_type, path, path_regex) = match &m.path {
        None => (routing::PathMatchType::Prefix, "/".to_string(), None),
        Some(p) if p.match_type == HttpPathMatchType::RegularExpression => (
            routing::PathMatchType::RegularExpression,
            p.value.clone(),
            Some(compile_regex(&p.value, "path")?),
        ),
        Some(p) => {
            if !p.value.starts_with('/') {
                return Err(RouteError::unsupported(format!(
                    "Path match value must start with '/': {}",
                    p.value
                )));
            }
            if p.match_type == HttpPathMatchType::Exact {
                (routing::PathMatchType::Exact, p.value.clone(), None)
            } else {
                // Gateway API v1.4.1 PathPrefix: "When specified, a trailing
                // `/` is ignored", so /abc/ matches /abc. The root stays "/".
                let prefix = match p.value.trim_end_matches('/') {
                    "" => "/",
                    trimmed => trimmed,
                };
                (routing::PathMatchType::Prefix, prefix.to_string(), None)
            }
        }
    };

    let headers = m
        .headers
        .iter()
        .enumerate()
        .map(|(i, h)| compile_header_match(h).map_err(|e| e.at(format_args!("header {}", i))))
        .collect::<Result<_, _>>()?;
    let query_params = m
        .query_params
        .iter()
        .enumerate()
        .map(|(i, q)| compile_query_match(q).map_err(|e| e.at(format_args!("query param {}", i))))
        .collect::<Result<_, _>>()?;

    Ok(CompiledMatch {
        method: m.method.clone(),
        path_match_type,
        path,
        path_regex,
        headers,
        query_params,
    })
}

fn compile_header_match(h: &HttpHeaderMatch) -> Result<routing::HeaderMatch, RouteError> {
    if h.name.is_empty() {
        return Err(RouteError::unsupported("Header match name cannot be empty"));
    }
    if !super::wire::is_header_name(h.name.as_bytes()) {
        return Err(RouteError::unsupported(format!(
            "Header match name '{}' contains invalid characters",
            h.name
        )));
    }
    Ok(routing::HeaderMatch {
        name: h.name.to_ascii_lowercase(),
        matcher: value_matcher(&h.value, &h.match_type, "header match")?,
    })
}

fn compile_query_match(q: &HttpQueryParamMatch) -> Result<routing::QueryParamMatch, RouteError> {
    if q.name.is_empty() {
        return Err(RouteError::unsupported("Query param name cannot be empty"));
    }
    // The data plane splits the query on '&' and '=': such a name never matches.
    if q.name.contains('&') || q.name.contains('=') {
        return Err(RouteError::unsupported(format!(
            "Query param name '{}' must not contain '&' or '='",
            q.name
        )));
    }
    Ok(routing::QueryParamMatch {
        name: q.name.clone(),
        matcher: value_matcher(&q.value, &q.match_type, "query param match")?,
    })
}

fn value_matcher(
    value: &str,
    match_type: &StringMatchType,
    what: &str,
) -> Result<routing::ValueMatcher, RouteError> {
    Ok(match match_type {
        StringMatchType::Exact => routing::ValueMatcher::Exact(value.to_string()),
        StringMatchType::RegularExpression => {
            routing::ValueMatcher::Regex(compile_regex(value, what)?)
        }
    })
}

/// Gateway API leaves the dialect to the implementation: this is the regex
/// crate's. A pattern matches the whole value, as in Envoy-based
/// implementations, so path regex `/api` does not match `/static/api/x`.
fn compile_regex(pattern: &str, what: &str) -> Result<regex::Regex, RouteError> {
    let invalid = |e: &dyn fmt::Display| {
        RouteError::unsupported(format!("Invalid {} regex '{}': {}", what, pattern, e))
    };
    // Alone first: a stray ')' would close the anchoring group and turn an
    // invalid pattern into a valid one.
    regex_syntax::ast::parse::Parser::new()
        .parse(pattern)
        .map_err(|e| invalid(&e))?;
    regex::Regex::new(&format!("^(?:{})$", pattern)).map_err(|e| invalid(&e))
}

fn compile_filter(f: &HttpRouteFilter) -> Result<CompiledFilter<'_>, RouteError> {
    match f {
        HttpRouteFilter::RequestRedirect { config } => match config.status_code {
            301 | 302 | 303 | 307 | 308 => {}
            code => {
                return Err(RouteError::unsupported(format!(
                    "RequestRedirect status_code must be 301, 302, 303, 307, or 308, got {}",
                    code
                )))
            }
        },
        HttpRouteFilter::RequestMirror { config } => config
            .backend_ref
            .validate()
            .map_err(RouteError::unsupported)?,
        _ => {}
    }
    // Every config path reaches the wire through here: no CR/LF or framing
    // header gets past, validate() or not.
    super::wire::check_filter(f).map_err(RouteError::unsupported)?;
    table_filter(f)
}

fn compile_backend_filters(b: &BackendRef) -> Result<Vec<routing::HttpFilter>, RouteError> {
    check_no_repeats(&b.filters)?;
    b.filters
        .iter()
        .enumerate()
        .map(|(i, f)| compile_backend_filter(f).map_err(|e| e.at(format_args!("filter {}", i))))
        .collect()
}

/// The data plane applies only header modifiers per backend: any other
/// filter there would be accepted and then silently never run.
fn compile_backend_filter(f: &HttpRouteFilter) -> Result<routing::HttpFilter, RouteError> {
    let unsupported = || {
        RouteError::unsupported(
            "only RequestHeaderModifier and ResponseHeaderModifier are supported on a backend ref",
        )
    };
    if !matches!(
        f,
        HttpRouteFilter::RequestHeaderModifier { .. }
            | HttpRouteFilter::ResponseHeaderModifier { .. }
    ) {
        return Err(unsupported());
    }
    super::wire::check_filter(f).map_err(RouteError::unsupported)?;
    match table_filter(f)? {
        CompiledFilter::Ready(filter) => Ok(filter),
        CompiledFilter::Mirror { .. } => Err(unsupported()),
    }
}

/// A checked filter in the table's form.
fn table_filter(f: &HttpRouteFilter) -> Result<CompiledFilter<'_>, RouteError> {
    use std::sync::Arc;

    Ok(CompiledFilter::Ready(match f {
        HttpRouteFilter::RequestHeaderModifier { config } => {
            routing::HttpFilter::RequestHeaderModifier {
                add: Arc::new(config.add.clone()),
                set: Arc::new(config.set.clone()),
                remove: Arc::new(config.remove.clone()),
            }
        }
        HttpRouteFilter::ResponseHeaderModifier { config } => {
            routing::HttpFilter::ResponseHeaderModifier {
                add: Arc::new(config.add.clone()),
                set: Arc::new(config.set.clone()),
                remove: Arc::new(config.remove.clone()),
            }
        }
        HttpRouteFilter::RequestRedirect { config } => routing::HttpFilter::RequestRedirect {
            // Accepted in any case, but the redirect builder compares
            // against lowercase literals to drop default ports.
            scheme: config.scheme.as_deref().map(str::to_ascii_lowercase),
            hostname: config.hostname.clone(),
            port: config.port,
            path: config.path.as_ref().map(routing::URLRewritePath::from),
            status_code: config.status_code,
        },
        HttpRouteFilter::URLRewrite { config } => routing::HttpFilter::URLRewrite {
            hostname: config.hostname.clone(),
            path: config.path.as_ref().map(routing::URLRewritePath::from),
        },
        HttpRouteFilter::RequestMirror { config } => {
            return Ok(CompiledFilter::Mirror {
                backend: &config.backend_ref,
                fraction: mirror_fraction(config)?,
            })
        }
    }))
}

/// Gateway API v1.4.1 bounds: percent 0 to 100, or a fraction whose
/// denominator is at least 1 and whose numerator is no larger. Neither
/// set mirrors every request; the fraction wins when both are.
fn mirror_fraction(config: &RequestMirrorConfig) -> Result<routing::Fraction, RouteError> {
    let (numerator, denominator) = match (&config.fraction, config.percent) {
        (Some(f), _) => (f.numerator, f.denominator),
        (None, Some(percent)) => (percent, 100),
        (None, None) => return Ok(routing::Fraction::ALL),
    };
    routing::Fraction::new(numerator, denominator).ok_or_else(|| {
        RouteError::unsupported(format!(
            "RequestMirror share {}/{} is not a fraction of requests: the denominator must be at least 1 and the numerator no larger",
            numerator, denominator
        ))
    })
}

impl From<&HttpURLRewritePath> for routing::URLRewritePath {
    fn from(p: &HttpURLRewritePath) -> Self {
        match p {
            HttpURLRewritePath::ReplaceFullPath { value } => Self::ReplaceFullPath(value.clone()),
            HttpURLRewritePath::ReplacePrefixMatch { value } => {
                Self::ReplacePrefixMatch(value.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_regex_is_judged_alone_before_it_is_anchored() {
        // Anchored as is, "/a)(/b" reads ^(?:/a)(/b)$: valid, and wrong.
        let err = compile_regex("/a)(/b", "path").unwrap_err();
        assert_eq!(err.reason, RejectReason::UnsupportedValue);
        assert!(
            err.message.starts_with("Invalid path regex '/a)(/b'"),
            "{}",
            err.message
        );

        // The group keeps an alternation whole: ^/api|/v2$ would match "/x/v2".
        let re = compile_regex("/api|/v[0-9]+", "path").unwrap();
        assert!(re.is_match("/api") && re.is_match("/v2"));
        assert!(!re.is_match("/x/v2") && !re.is_match("/api/x"));
        assert!(compile_regex("^/a.*$", "path").unwrap().is_match("/abc"));
        assert!(compile_regex("(?i)/api", "path").unwrap().is_match("/API"));
    }

    fn mirror(percent: Option<u32>, fraction: Option<(u32, u32)>) -> RequestMirrorConfig {
        RequestMirrorConfig {
            backend_ref: serde_json::from_str(r#"{"name":"m.svc","port":80}"#).unwrap(),
            percent,
            fraction: fraction.map(|(numerator, denominator)| MirrorFraction {
                numerator,
                denominator,
            }),
        }
    }

    /// The CRD rejects these shares; a file config used to clamp them to
    /// 100%, and a zero denominator left nothing to draw from.
    #[test]
    fn a_mirror_share_outside_the_spec_bounds_is_rejected() {
        for config in [
            mirror(Some(101), None),
            mirror(None, Some((3, 2))),
            mirror(None, Some((0, 0))),
        ] {
            let err = mirror_fraction(&config).unwrap_err();
            assert_eq!(err.reason, RejectReason::UnsupportedValue);
            assert!(
                err.message.starts_with("RequestMirror share"),
                "{}",
                err.message
            );
        }
        let edge = |p, f| mirror_fraction(&mirror(p, f)).unwrap();
        assert_eq!(edge(Some(0), None), routing::Fraction::new(0, 100).unwrap());
        assert_eq!(edge(Some(100), None), routing::Fraction::new(1, 1).unwrap());
        assert_eq!(edge(None, Some((7, 7))), routing::Fraction::ALL);
    }
}
