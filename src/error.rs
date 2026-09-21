use solana_client::client_error::ClientError;

#[derive(Debug, thiserror::Error)]
pub enum TradeError {
    #[error("config: invalid {what} URL {url:?} — {detail}")]
    Config {
        what: &'static str,
        url: String,
        detail: String,
    },

    #[error("{venue} API returned HTTP {status}: {body}")]
    Http {
        venue: &'static str,
        status: u16,
        body: String,
    },

    #[error("network: {context}: {source}")]
    Network {
        context: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("rpc: {context}: {source}")]
    Rpc {
        context: &'static str,
        #[source]
        source: ClientError,
    },

    #[error("{0}: malformed API response: {1}")]
    Decode(&'static str, String),

    #[error("build transaction: {0}")]
    Build(String),

    #[error("transaction simulation failed: {error}; logs: {logs:?}")]
    Simulation { error: String, logs: Vec<String> },

    #[error("{venue} swap failed: {msg}")]
    Venue { venue: &'static str, msg: String },

    #[error("sign: {0}")]
    Sign(String),

    #[error("submit: {0}")]
    Submit(String),
}

pub type Result<T> = std::result::Result<T, TradeError>;
