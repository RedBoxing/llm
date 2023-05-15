//! This crate provides a unified interface for loading and using
//! Large Language Models (LLMs). The following models are supported:
//!
//! - [BLOOM](llm_bloom)
//! - [GPT-2](llm_gpt2)
//! - [GPT-J](llm_gptj)
//! - [LLaMA](llm_llama)
//! - [GPT-NeoX](llm_neox)
//!
//! At present, the only supported backend is [GGML](https://github.com/ggerganov/ggml), but this is expected to
//! change in the future.
//!
//! # Example
//!
//! ```no_run
//! use std::io::Write;
//! use llm::Model;
//!
//! // load a GGML model from disk
//! let llama = llm::load::<llm::models::Llama>(
//!     // path to GGML file
//!     std::path::Path::new("/path/to/model"),
//!     // llm::ModelParameters
//!     Default::default(),
//!     // load progress callback
//!     llm::load_progress_callback_stdout
//! )
//! .unwrap_or_else(|err| panic!("Failed to load model: {err}"));
//!
//! // use the model to generate text from a prompt
//! let mut session = llama.start_session(Default::default());
//! let res = session.infer::<std::convert::Infallible>(
//!     // model to use for text generation
//!     &llama,
//!     // randomness provider
//!     &mut rand::thread_rng(),
//!     // the prompt to use for text generation, as well as other
//!     // inference parameters
//!     &llm::InferenceRequest {
//!         prompt: "Rust is a cool programming language because",
//!         ..Default::default()
//!     },
//!     // llm::OutputRequest
//!     &mut Default::default(),
//!     // output callback
//!     |t| {
//!         print!("{t}");
//!         std::io::stdout().flush().unwrap();
//!
//!         Ok(())
//!     }
//! );
//!
//! match res {
//!     Ok(result) => println!("\n\nInference stats:\n{result}"),
//!     Err(err) => println!("\n{err}"),
//! }
//! ```
#![deny(missing_docs)]

use std::{
    error::Error,
    fmt::{Debug, Display},
    path::Path,
    str::FromStr,
};

// Try not to expose too many GGML details here.
// This is the "user-facing" API, and GGML may not always be our backend.
pub use llm_base::{
    ggml::format as ggml_format, load, load_progress_callback_stdout, quantize, ElementType,
    FileType, InferenceError, InferenceParameters, InferenceRequest, InferenceSession,
    InferenceSessionConfig, InferenceSnapshot, InvalidTokenBias, KnownModel, LoadError,
    LoadProgress, Loader, Model, ModelKVMemoryType, ModelParameters, OutputRequest, QuantizeError,
    QuantizeProgress, SnapshotError, TokenBias, TokenId, TokenUtf8Buffer, Vocabulary,
};
use serde::Serialize;

/// All available models.
pub mod models {
    #[cfg(feature = "bloom")]
    pub use llm_bloom::{self as bloom, Bloom};
    #[cfg(feature = "gpt2")]
    pub use llm_gpt2::{self as gpt2, Gpt2};
    #[cfg(feature = "gptj")]
    pub use llm_gptj::{self as gptj, GptJ};
    #[cfg(feature = "llama")]
    pub use llm_llama::{self as llama, Llama};
    #[cfg(feature = "neox")]
    pub use llm_neox::{self as neox, NeoX};
    #[cfg(feature = "rwkv")]
    pub use llm_rwkv::{self as rwkv, Rwkv};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
/// All available model architectures.
pub enum ModelArchitecture {
    #[cfg(feature = "bloom")]
    /// [BLOOM](llm_bloom)
    Bloom,
    #[cfg(feature = "gpt2")]
    /// [GPT-2](llm_gpt2)
    Gpt2,
    #[cfg(feature = "gptj")]
    /// [GPT-J](llm_gptj)
    GptJ,
    #[cfg(feature = "llama")]
    /// [LLaMA](llm_llama)
    Llama,
    #[cfg(feature = "neox")]
    /// [GPT-NeoX](llm_neox)
    NeoX,
    #[cfg(feature = "rwkv")]
    /// [RWKV](llm_rwkv)
    Rwkv,
}

impl ModelArchitecture {
    /// All available model architectures
    pub const ALL: [Self; 6] = [
        Self::Bloom,
        Self::Gpt2,
        Self::GptJ,
        Self::Llama,
        Self::NeoX,
        Self::Rwkv,
    ];
}

/// An unsupported model architecture was specified.
pub struct UnsupportedModelArchitecture(String);
impl Display for UnsupportedModelArchitecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for UnsupportedModelArchitecture {}

impl Debug for UnsupportedModelArchitecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("UnsupportedModelArchitecture")
            .field(&self.0)
            .finish()
    }
}

impl FromStr for ModelArchitecture {
    type Err = UnsupportedModelArchitecture;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use ModelArchitecture::*;
        match s
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
            .as_str()
        {
            #[cfg(feature = "bloom")]
            "bloom" => Ok(Bloom),
            #[cfg(feature = "gpt2")]
            "gpt2" => Ok(Gpt2),
            #[cfg(feature = "gptj")]
            "gptj" => Ok(GptJ),
            #[cfg(feature = "llama")]
            "llama" => Ok(Llama),
            #[cfg(feature = "neox")]
            "gptneox" => Ok(NeoX),
            #[cfg(feature = "rwkv")]
            "rwkv" => Ok(Rwkv),
            m => Err(UnsupportedModelArchitecture(format!(
                "{m} is not a supported model architecture"
            ))),
        }
    }
}

impl Display for ModelArchitecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ModelArchitecture::*;

        match self {
            #[cfg(feature = "bloom")]
            Bloom => write!(f, "BLOOM"),
            #[cfg(feature = "gpt2")]
            Gpt2 => write!(f, "GPT-2"),
            #[cfg(feature = "gptj")]
            GptJ => write!(f, "GPT-J"),
            #[cfg(feature = "llama")]
            Llama => write!(f, "LLaMA"),
            #[cfg(feature = "neox")]
            NeoX => write!(f, "GPT-NeoX"),
            #[cfg(feature = "rwkv")]
            Rwkv => write!(f, "RWKV"),
        }
    }
}

/// A helper function that loads the specified model from disk using an architecture
/// specified at runtime.
///
/// A wrapper around [load] that dispatches to the correct model.
pub fn load_dynamic(
    architecture: ModelArchitecture,
    path: &Path,
    vocab_path: &Path,
    params: ModelParameters,
    load_progress_callback: impl FnMut(LoadProgress),
) -> Result<Box<dyn Model>, LoadError> {
    use ModelArchitecture::*;

    let model: Box<dyn Model> = match architecture {
        #[cfg(feature = "bloom")]
        Bloom => Box::new(load::<models::Bloom>(
            path,
            vocab_path,
            params,
            load_progress_callback,
        )?),
        #[cfg(feature = "gpt2")]
        Gpt2 => Box::new(load::<models::Gpt2>(
            path,
            vocab_path,
            params,
            load_progress_callback,
        )?),
        #[cfg(feature = "gptj")]
        GptJ => Box::new(load::<models::GptJ>(
            path,
            vocab_path,
            params,
            load_progress_callback,
        )?),
        #[cfg(feature = "llama")]
        Llama => Box::new(load::<models::Llama>(
            path,
            vocab_path,
            params,
            load_progress_callback,
        )?),
        #[cfg(feature = "neox")]
        NeoX => Box::new(load::<models::NeoX>(
            path,
            vocab_path,
            params,
            load_progress_callback,
        )?),
        #[cfg(feature = "rwkv")]
        Rwkv => Box::new(load::<models::Rwkv>(
            path,
            vocab_path,
            params,
            load_progress_callback,
        )?),
    };

    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_architecture_from_str() {
        for arch in &ModelArchitecture::ALL {
            assert_eq!(
                arch,
                &arch.to_string().parse::<ModelArchitecture>().unwrap()
            );
        }
    }
}
