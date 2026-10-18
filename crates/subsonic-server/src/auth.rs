//! The protocol's sign-in: `u` with `t` + `s` (token = md5(password + salt)) or with `p`
//! (plain or `enc:<hex>`). API keys are refused with code 42 so clients fall back.

use md5::{Digest, Md5};

use crate::Error;

/// `(user, password)` the server expects.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

/// Subsonic's token: `md5(password + salt)` in lowercase hex.
pub fn token(password: &str, salt: &str) -> String {
    let mut h = Md5::new();
    h.update(password.as_bytes());
    h.update(salt.as_bytes());
    format!("{:x}", h.finalize())
}

/// The `p` parameter, plain or `enc:<hex>`.
pub(crate) fn plain(p: &str) -> String {
    let decode = |h: &str| {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
            .collect::<Option<Vec<u8>>>()
            .and_then(|b| String::from_utf8(b).ok())
    };
    p.strip_prefix("enc:").and_then(decode).unwrap_or_else(|| p.to_string())
}

/// `Ok(signed_in)`. No credentials configured: nobody is signed in and nothing is checked.
/// `trusted` (your own middleware vouched for the request) passes without a check.
pub(crate) fn authorize(
    creds: Option<&Credentials>,
    trusted: bool,
    u: Option<&str>,
    t: Option<&str>,
    s: Option<&str>,
    p: Option<&str>,
    api_key: Option<&str>,
) -> Result<bool, Error> {
    let Some(creds) = creds else { return Ok(trusted) };
    if trusted {
        return Ok(true);
    }
    if api_key.is_some() {
        return Err(Error::new(42, "API keys are not supported; sign in with username and password"));
    }
    let u = u.ok_or_else(|| Error::missing("u"))?;
    let ok = match (t, s, p) {
        (Some(t), Some(salt), _) => t.eq_ignore_ascii_case(&token(&creds.password, salt)),
        (None, None, Some(pw)) => plain(pw) == creds.password,
        _ => return Err(Error::new(10, "missing parameters t and s (or p)")),
    };
    if u != creds.user || !ok {
        return Err(Error::new(40, "Wrong username or password"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_matches_the_spec_example() {
        // From the Subsonic API docs: password "sesame", salt "c19b2d".
        assert_eq!(token("sesame", "c19b2d"), "26719a1196d2a940705a59634eb18eab");
        assert_eq!(plain("enc:736573616d65"), "sesame");
        assert_eq!(plain("sesame"), "sesame");
    }

    #[test]
    fn anonymous_without_credentials_and_strict_with_them() {
        assert!(!authorize(None, false, Some("x"), None, None, Some("nope"), None).unwrap());
        assert!(authorize(None, true, None, None, None, None, None).unwrap());

        let c = Credentials { user: "me".into(), password: "sesame".into() };
        let auth = |u, t, s, p, k| authorize(Some(&c), false, u, t, s, p, k);
        assert!(auth(Some("me"), Some("26719a1196d2a940705a59634eb18eab"), Some("c19b2d"), None, None).unwrap());
        assert!(auth(Some("me"), None, None, Some("enc:736573616d65"), None).unwrap());
        assert!(authorize(Some(&c), true, None, None, None, None, None).unwrap(), "trusted passes");
        let code = |u, t, s, p, k| auth(u, t, s, p, k).err().map(|e| e.code);
        assert_eq!(code(Some("me"), None, None, Some("wrong"), None), Some(40));
        assert_eq!(code(Some("you"), None, None, Some("sesame"), None), Some(40));
        assert_eq!(code(Some("me"), None, None, None, None), Some(10));
        assert_eq!(code(None, None, None, None, Some("k")), Some(42));
    }
}
