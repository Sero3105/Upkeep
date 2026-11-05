use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bump {
    None,
    Patch,
    Minor,
    Major,
    Unknown,
}

/// Numeric components: "v3.14.7_1" -> [3, 14, 7, 1].
pub fn parts(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s[..s.len().min(18)].parse().ok())
        .collect()
}

pub fn compare(a: &str, b: &str) -> Ordering {
    let (pa, pb) = (parts(a), parts(b));
    if pa.is_empty() && pb.is_empty() {
        return a.cmp(b);
    }
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).unwrap_or(&0), pb.get(i).unwrap_or(&0));
        if x != y {
            return x.cmp(y);
        }
    }
    Ordering::Equal
}

pub fn is_newer(candidate: &str, than: &str) -> bool {
    compare(candidate, than) == Ordering::Greater
}

pub fn bump(from: &str, to: &str) -> Bump {
    let (a, b) = (parts(from), parts(to));
    if a.is_empty() || b.is_empty() {
        return if from == to { Bump::None } else { Bump::Unknown };
    }
    for i in 0..a.len().max(b.len()) {
        if a.get(i).unwrap_or(&0) != b.get(i).unwrap_or(&0) {
            return match i {
                0 => Bump::Major,
                1 => Bump::Minor,
                _ => Bump::Patch,
            };
        }
    }
    Bump::None
}

/// First dotted version in a `--version` line: "git version 2.50.1 (Apple Git-155)" -> "2.50.1".
pub fn extract(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // Letters may prefix a version ("v1.2", "go1.22.0"); never start mid-number.
        if b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.')) {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            let tok = text[start..i].trim_end_matches('.');
            if tok.contains('.') {
                return Some(tok.to_string());
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_numerically() {
        assert_eq!(compare("1.10.0", "1.9.9"), Ordering::Greater);
        assert_eq!(compare("78.3", "78.3_1"), Ordering::Less);
        assert_eq!(compare("2.0", "2.0.0"), Ordering::Equal);
        assert!(is_newer("157.0.8103.39", "150.0.7871.47"));
        assert!(!is_newer("1.0", "1.0"));
    }

    #[test]
    fn classifies_bumps() {
        assert_eq!(bump("1.2.3", "2.0.0"), Bump::Major);
        assert_eq!(bump("3.12.1", "3.13.0"), Bump::Minor);
        assert_eq!(bump("1.4.7", "1.4.8"), Bump::Patch);
        assert_eq!(bump("78.3", "78.3_1"), Bump::Patch);
        assert_eq!(bump("latest", "nightly"), Bump::Unknown);
        assert_eq!(bump("1.0", "1.0"), Bump::None);
    }

    #[test]
    fn extracts_versions() {
        assert_eq!(extract("git version 2.50.1 (Apple Git-155)").as_deref(), Some("2.50.1"));
        assert_eq!(extract("2.1.318 (Claude Code)").as_deref(), Some("2.1.318"));
        assert_eq!(extract("Terraform v1.9.0").as_deref(), Some("1.9.0"));
        assert_eq!(extract("OpenSSL 3.6.0 1 Oct 2025").as_deref(), Some("3.6.0"));
        assert_eq!(extract("x86_64 build"), None);
        assert_eq!(extract("go1.22.0").as_deref(), Some("1.22.0"));
        assert_eq!(extract("cpython 3.14.7 macos").as_deref(), Some("3.14.7"));
    }
}
