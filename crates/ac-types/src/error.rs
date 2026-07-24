/// Completion failure taxonomy. Variants are what retry/backoff logic keys on,
/// so keep them semantic (what happened), not transport-shaped (how we learned).
#[derive(Debug, thiserror::Error)]
pub enum CompletionError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited{}", retry_after_ms.map(|ms| format!(" (retry after {ms} ms)")).unwrap_or_default())]
    RateLimited { retry_after_ms: Option<u64> },
    /// The account is out of credit (HTTP 402). Distinct from `Auth`: the key
    /// is valid, and no amount of retrying helps — the host must surface a
    /// top-up path to the user.
    #[error("insufficient credits: {0}")]
    InsufficientCredits(String),
    #[error("provider overloaded: {0}")]
    Overloaded(String),
    #[error("prompt too large: {0}")]
    PromptTooLarge(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("stream parse error: {0}")]
    Parse(String),
    #[error("provider error: {0}")]
    Other(String),
}
