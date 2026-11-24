//! FreeArc method strings: "xt3uo+srep:m3yf+magic2" -> [xt3uo, srep(m3yf), magic2].
//! Compression applies the chain left to right, so decompression runs it right to left.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub name: String,
    pub params: Vec<String>,
}

impl Method {
    /// Parameters joined back with ':' ("m3yf", "mfbt4:d1m").
    pub fn params_str(&self) -> String {
        self.params.join(":")
    }

    /// The method as written in the archive ("srep:m3yf").
    pub fn full(&self) -> String {
        if self.params.is_empty() { self.name.clone() } else { format!("{}:{}", self.name, self.params_str()) }
    }
}

pub fn parse_chain(s: &str) -> Vec<Method> {
    s.split('+')
        .filter(|m| !m.is_empty())
        .map(|m| {
            let mut parts = m.split(':');
            let name = parts.next().unwrap_or_default().to_string();
            Method { name, params: parts.filter(|p| !p.is_empty()).map(str::to_string).collect() }
        })
        .collect()
}

/// Parses FreeArc sizes like "1m", "64mb", "512k", "1g", "4096", "100b".
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.to_ascii_lowercase();
    let s = s.strip_suffix('b').filter(|r| !r.is_empty() && r.ends_with(['k', 'm', 'g'])).unwrap_or(&s);
    let (num, mult) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1u64 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        'b' => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    num.parse::<u64>().ok().map(|n| n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain() {
        let c = parse_chain("xt3uo+srep:m3yf+magic2");
        assert_eq!(c.len(), 3);
        assert_eq!(c[1].name, "srep");
        assert_eq!(c[1].params, vec!["m3yf"]);
        assert_eq!(c[1].full(), "srep:m3yf");
        assert_eq!(c[2].full(), "magic2");
        assert_eq!(parse_chain("lzma:mfbt4:d1m")[0].params_str(), "mfbt4:d1m");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1m"), Some(1 << 20));
        assert_eq!(parse_size("64mb"), Some(64 << 20));
        assert_eq!(parse_size("512k"), Some(512 << 10));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("100b"), Some(100));
        assert_eq!(parse_size("fast"), None);
    }
}
