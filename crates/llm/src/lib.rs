//! This crate provides a unified interface for loading and using
//! Large Language Models (LLMs). The following models are supported:
//!
//! - [BLOOM](llm_bloom)
//! - [GPT-2](llm_gpt2)
//! - [GPT-J](llm_gptj)
//! - [LLaMA](llm_llama)
//! - [GPT-NeoX](llm_gptneox)
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
//!     // optional path to a vocabulary file
//!     None,
//!     // llm::ModelParameters
//!     Default::default(),
//!     // llm::KnownModel::Overrides
//!     None,
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
//!     |r| match r {
//!         llm::InferenceResponse::PromptToken(t) | llm::InferenceResponse::InferredToken(t) => {
//!             print!("{t}");
//!             std::io::stdout().flush().unwrap();
//!
//!             Ok(llm::InferenceFeedback::Continue)
//!         }
//!         _ => Ok(llm::InferenceFeedback::Continue),
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
    feed_prompt_callback, ggml::format as ggml_format, load, load_progress_callback_stdout,
    quantize, ElementType, FileType, FileTypeFormat, InferenceError, InferenceFeedback,
    InferenceParameters, InferenceRequest, InferenceResponse, InferenceSession,
    InferenceSessionConfig, InferenceSnapshot, InferenceStats, InvalidTokenBias, KnownModel,
    LoadError, LoadProgress, Loader, Model, ModelDynamicOverrideValue, ModelDynamicOverrides,
    ModelKVMemoryType, ModelParameters, OutputRequest, QuantizeError, QuantizeProgress,
    SnapshotError, TokenBias, TokenId, TokenUtf8Buffer, Vocabulary,
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
    #[cfg(feature = "gptneox")]
    pub use llm_gptneox::{self as gptneox, GptNeoX, GptNeoXOverrides};
    #[cfg(feature = "llama")]
    pub use llm_llama::{self as llama, Llama};
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
    #[cfg(feature = "gptneox")]
    /// [GPT-NeoX](llm_gptneox)
    GptNeoX,
    #[cfg(feature = "gptneox")]
    /// RedPajama: [GPT-NeoX](llm_gptneox) with `use_parallel_residual` set to false
    RedPajama,
    #[cfg(feature = "rwkv")]
    /// [RWKV](llm_rwkv)
    Rwkv,
}

impl ModelArchitecture {
    /// All available model architectures
    pub const ALL: [Self; 7] = [
        #[cfg(feature = "bloom")]
        Self::Bloom,
        #[cfg(feature = "gpt2")]
        Self::Gpt2,
        #[cfg(feature = "gptj")]
        Self::GptJ,
        #[cfg(feature = "llama")]
        Self::Llama,
        #[cfg(feature = "gptneox")]
        Self::GptNeoX,
        #[cfg(feature = "gptneox")]
        Self::RedPajama,
        #[cfg(feature = "rwkv")]
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
            #[cfg(feature = "gptneox")]
            "gptneox" => Ok(GptNeoX),
            #[cfg(feature = "gptneox")]
            "redpajama" => Ok(RedPajama),
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
            #[cfg(feature = "gptneox")]
            GptNeoX => write!(f, "GPT-NeoX"),
            #[cfg(feature = "gptneox")]
            RedPajama => write!(f, "RedPajama"),
            #[cfg(feature = "rwkv")]
            Rwkv => write!(f, "RWKV"),
        }
    }
}

/// A helper function that loads the specified model from disk using an architecture
/// specified at runtime.
///
/// The `overrides` will attempt to deserialize to the [KnownModel::Overrides] type
/// for that model. If the model does not support overrides, this will be an empty
/// struct. If the overrides are invalid, this will return an error.
///
/// A wrapper around [load] that dispatches to the correct model.
pub fn load_dynamic(
    architecture: ModelArchitecture,
    path: &Path,
    vocabulary_path: Option<&Path>,
    params: ModelParameters,
    overrides: Option<ModelDynamicOverrides>,
    load_progress_callback: impl FnMut(LoadProgress),
) -> Result<Box<dyn Model>, LoadError> {
    use ModelArchitecture::*;

    fn load_model<M: KnownModel + 'static>(
        path: &Path,
        vocabulary_path: Option<&Path>,
        params: ModelParameters,
        overrides: Option<ModelDynamicOverrides>,
        load_progress_callback: impl FnMut(LoadProgress),
    ) -> Result<Box<dyn Model>, LoadError> {
        Ok(Box::new(load::<M>(
            path,
            vocabulary_path,
            params,
            overrides.map(|o| o.into()),
            load_progress_callback,
        )?))
    }

    let model: Box<dyn Model> = match architecture {
        #[cfg(feature = "bloom")]
        Bloom => load_model::<models::Bloom>(
            path,
            vocabulary_path,
            params,
            overrides,
            load_progress_callback,
        )?,
        #[cfg(feature = "gpt2")]
        Gpt2 => load_model::<models::Gpt2>(
            path,
            vocabulary_path,
            params,
            overrides,
            load_progress_callback,
        )?,
        #[cfg(feature = "gptj")]
        GptJ => load_model::<models::GptJ>(
            path,
            vocabulary_path,
            params,
            overrides,
            load_progress_callback,
        )?,
        #[cfg(feature = "llama")]
        Llama => load_model::<models::Llama>(
            path,
            vocabulary_path,
            params,
            overrides,
            load_progress_callback,
        )?,
        #[cfg(feature = "gptneox")]
        GptNeoX => load_model::<models::GptNeoX>(
            path,
            vocabulary_path,
            params,
            overrides,
            load_progress_callback,
        )?,
        #[cfg(feature = "gptneox")]
        RedPajama => load_model::<models::GptNeoX>(
            path,
            vocabulary_path,
            params,
            {
                let mut overrides = overrides.unwrap_or_default();
                overrides.merge(models::GptNeoXOverrides {
                    use_parallel_residual: false,
                });
                Some(overrides)
            },
            load_progress_callback,
        )?,
        #[cfg(feature = "rwkv")]
        Rwkv => load_model::<models::Rwkv>(
            path,
            vocabulary_path,
            params,
            overrides,
            load_progress_callback,
        )?,
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
