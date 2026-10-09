//! JSON POST shared by the Jev and LLM clients (laco_uia.py:131-145), and Python's `json.dumps`.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// A failure named like the Python exception it replaces, so messages keep the `<Tipo>: <msg>` shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Falha {
    pub tipo: &'static str,
    pub msg: String,
}

impl Falha {
    pub fn new(tipo: &'static str, msg: impl Into<String>) -> Falha {
        Falha { tipo, msg: msg.into() }
    }
}

impl fmt::Display for Falha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.tipo, self.msg)
    }
}

pub(crate) fn cliente() -> reqwest::Client {
    // The proxy env of the user's shell must not reroute Jev or the local LLM proxy.
    // No redirects: urllib refuses to re-POST on 307/308, and the body carries the screen state.
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("cliente HTTP sem proxy")
}

fn rede(e: reqwest::Error) -> Falha {
    if e.is_timeout() && !e.is_connect() {
        return Falha::new("TimeoutError", "timed out");
    }
    if e.is_timeout() {
        return Falha::new("URLError", "<urlopen error timed out>");
    }
    // The URL may carry credentials (user:pass@, ?key=); urllib's message never shows it.
    let sem_url = e.without_url();
    let mut msg = sem_url.to_string();
    let mut causa = std::error::Error::source(&sem_url);
    while let Some(c) = causa {
        msg = format!("{msg}: {c}");
        causa = c.source();
    }
    Falha::new("URLError", format!("<urlopen error {msg}>"))
}

/// POST `corpo` with `authorization: Bearer <chave>`; non-2xx becomes `HTTP <code> em <url>: <2000 bytes>`
/// with the key replaced by `[redigido]`.
pub async fn post(http: &reqwest::Client, url: &str, corpo: &Value, chave: &str, timeout: Duration) -> Result<Value, Falha> {
    let mut r = http
        .post(url)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {chave}"))
        .body(serde_json::to_vec(corpo).expect("Value serializa"))
        .timeout(timeout)
        .send()
        .await
        .map_err(rede)?;
    let status = r.status();
    if !status.is_success() {
        let limite = 2000 + chave.len();
        let mut bruto = Vec::new();
        while bruto.len() < limite {
            match r.chunk().await.map_err(rede)? {
                Some(c) => bruto.extend_from_slice(&c),
                None => break,
            }
        }
        bruto.truncate(limite);
        // Python cuts at byte 2000; a key straddling the cut is kept whole so it is redacted, never halved.
        let mut fim = bruto.len().min(2000);
        let k = chave.as_bytes();
        if let Some(i) = (fim.saturating_sub(k.len().saturating_sub(1))..fim).find(|&i| bruto[i..].starts_with(k)) {
            fim = i + k.len();
        }
        let mut detalhe = String::from_utf8_lossy(&bruto[..fim]).into_owned();
        if !chave.is_empty() {
            detalhe = detalhe.replace(chave, "[redigido]");
        }
        return Err(Falha::new("RuntimeError", format!("HTTP {} em {url}: {detalhe}", status.as_u16())));
    }
    let bytes = r.bytes().await.map_err(rede)?;
    serde_json::from_slice(&bytes).map_err(|e| Falha::new("JSONDecodeError", e.to_string()))
}

/// Python `float(v)` when `v` is a number (or bool) in [0, 1], else 0.
pub fn prob(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|p| (0.0..=1.0).contains(p)).unwrap_or(0.0),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        _ => 0.0,
    }
}

/// `json.dumps(v, ensure_ascii=False)`: `", "`/`": "` separators and Python float repr.
pub fn py_dumps(v: &Value) -> String {
    let mut s = String::new();
    escrever(v, &mut s);
    s
}

fn escrever(v: &Value, s: &mut String) {
    match v {
        Value::Number(n) if n.is_f64() => s.push_str(&py_float(n.as_f64().unwrap_or_default())),
        Value::Array(a) => {
            s.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                escrever(x, s);
            }
            s.push(']');
        }
        Value::Object(o) => {
            s.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&Value::from(k.as_str()).to_string());
                s.push_str(": ");
                escrever(x, s);
            }
            s.push('}');
        }
        // Strings: serde escapes exactly what ensure_ascii=False escapes.
        _ => s.push_str(&v.to_string()),
    }
}

/// Python `repr(float)`: shortest round-trip digits, exponent form outside 1e-4 <= |x| < 1e16.
fn py_float(f: f64) -> String {
    let e = format!("{f:e}");
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if f == 0.0 || !(-4..16).contains(&exp) {
        if f == 0.0 {
            return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
        }
        return format!("{mantissa}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    let s = f.to_string();
    if s.contains('.') { s } else { format!("{s}.0") }
}

#[cfg(test)]
mod testes {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_like_python_repr() {
        for (f, py) in [(1.0, "1.0"), (0.1, "0.1"), (1e-5, "1e-05"), (1e16, "1e+16"), (1.5e20, "1.5e+20"),
                        (1e-4, "0.0001"), (123456789012345.6, "123456789012345.6"), (-2.5e-7, "-2.5e-07"), (0.0, "0.0")] {
            assert_eq!(py_float(f), py, "{f}");
        }
    }

    #[test]
    fn dumps_like_python() {
        let v = json!({"a": [1, 2.0, "ç\"\n"], "b": {}, "c": null, "d": true});
        assert_eq!(py_dumps(&v), r#"{"a": [1, 2.0, "ç\"\n"], "b": {}, "c": null, "d": true}"#);
    }

    #[test]
    fn prob_clamps_like_python() {
        assert_eq!(prob(Some(&json!(0.5))), 0.5);
        assert_eq!(prob(Some(&json!(1))), 1.0);
        assert_eq!(prob(Some(&json!(true))), 1.0);
        assert_eq!(prob(Some(&json!(1.5))), 0.0);
        assert_eq!(prob(Some(&json!("0.5"))), 0.0);
        assert_eq!(prob(None), 0.0);
    }
}
