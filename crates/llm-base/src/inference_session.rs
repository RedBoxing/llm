use std::fmt::Display;

use partial_sort::PartialSort;
use rand::{distributions::WeightedIndex, prelude::Distribution};
use thiserror::Error;

use crate::{
    mulf, InferenceError, InferenceParameters, Model, OutputRequest, TokenId, TokenUtf8Buffer,
};

// The size of a scratch buffer used for inference. This is used for temporary
// storage of intermediate results during inference.
//
// The specific value was copied from `llama.cpp`.
const SCRATCH_SIZE: usize = 512 * 1024 * 1024;

/// An inference session represents the state of the text generation. This holds
/// the full context window, as well as several additional parameters used
/// during sampling.
///
/// # Safety
/// This implements `Send` as it can be sent to another thread. However, it does
/// not implement `Sync` - it *cannot* be used from multiple threads at the same time.
///
/// Consider spawning multiple inference sessions for the same model if you need
/// to use it from multiple threads.
pub struct InferenceSession {
    // Must be kept alive for the model
    pub(crate) _session_ctx: ggml::Context,

    // Original size of the memory used to create this context.
    pub(crate) memory_size: usize,

    // Configuration for the session.
    pub(crate) config: InferenceSessionConfig,

    /// Memory K
    #[doc(hidden)]
    pub memory_k: ggml::Tensor,

    /// Memory M
    #[doc(hidden)]
    pub memory_v: ggml::Tensor,

    /// RWKV's State
    #[doc(hidden)]
    pub state: ggml::Tensor,

    /// How many tokens have been fed into the model's working memory so far.
    #[doc(hidden)]
    pub n_past: usize,

    /// How much memory is required per token for the temporary context used
    /// during inference.
    #[doc(hidden)]
    pub mem_per_token: usize,

    /// All tokens generated by this inference session
    pub(crate) tokens: Vec<TokenId>,

    /// The logits that were last predicted by the network. Zeroed out otherwise.
    #[doc(hidden)]
    pub last_logits: Vec<f32>,

    /// Scratch buffers used during inference.
    ///
    /// The number of scratch buffers was copied from `llama.cpp`.
    /// There is no specific reason for this number, but one is insufficient.
    #[doc(hidden)]
    pub scratch: [ggml::Buffer; 2],
}
unsafe impl Send for InferenceSession {}
impl InferenceSession {
    /// Feed a prompt to the model for this session.
    pub fn feed_prompt<E: std::error::Error + 'static>(
        &mut self,
        model: &dyn Model,
        params: &InferenceParameters,
        prompt: &str,
        output_request: &mut OutputRequest,
        mut callback: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), InferenceError> {
        let beginning_of_sentence = self.n_past == 0;

        let vocab = model.vocabulary();
        let prompt_tokens: Vec<TokenId> = vocab
            .tokenize(prompt, beginning_of_sentence)?
            .iter()
            .map(|(_, tok)| *tok)
            .collect();

        if self.n_past + prompt_tokens.len() >= model.n_context_tokens() {
            return Err(InferenceError::ContextFull);
        }

        for batch in prompt_tokens.chunks(params.n_batch) {
            model.evaluate(self, params, batch, output_request);
            for &tk in batch {
                let should_call_callback = Some(tk) != model.bot_token_id();

                if should_call_callback {
                    // NOTE: No string ever tokenizes to the end of sentence. So we
                    // can just return the id here.
                    if let Err(e) = callback(vocab.token(tk as usize)) {
                        return Err(InferenceError::UserCallback(Box::new(e)));
                    }
                }

                // Update the tokens for this session
                self.tokens.push(tk);
            }
        }

        Ok(())
    }

    /// Infer the next token for this session.
    pub fn infer_next_token<'v>(
        &mut self,
        model: &'v dyn Model,
        params: &InferenceParameters,
        output_request: &mut OutputRequest,
        rng: &mut impl rand::Rng,
    ) -> Result<&'v [u8], InferenceError> {
        if self.n_past + 1 >= model.n_context_tokens() {
            return Err(InferenceError::ContextFull);
        }

        // First, sample the next token, using the stored last_logits;
        let next_token = self.sample_top_p_top_k(params, rng);

        // Update the tokens for this session
        self.tokens.push(next_token);

        // Then, evaluate the network again to compute the new last_logits
        model.evaluate(self, params, &[next_token], output_request);

        // Return the next token
        if next_token as TokenId == model.eot_token_id() {
            Err(InferenceError::EndOfText)
        } else {
            Ok(model.vocabulary().token(next_token as usize))
        }
    }

    /// Generate text by using the provided [Model] to evaluate the `prompt`.
    ///
    /// The `callback` is called with each new token until an end-of-text (EOT)
    /// token is encountered or the maximum number of tokens have been
    /// generated (specified by [InferenceRequest::maximum_token_count]).
    ///
    /// This is a wrapper around [Self::feed_prompt] and [Self::infer_next_token].
    pub fn infer<E: std::error::Error + 'static>(
        &mut self,
        model: &dyn Model,
        rng: &mut impl rand::Rng,
        request: &InferenceRequest,
        output_request: &mut OutputRequest,
        mut callback: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<InferenceStats, InferenceError> {
        let maximum_token_count = request.maximum_token_count.unwrap_or(usize::MAX);
        if request.play_back_previous_tokens {
            // "Play back" the existing tokens, so that loading from an inference snapshot works
            // as expected.
            let mut token_utf8_buf = TokenUtf8Buffer::new();
            for token_id in &self.tokens {
                // Buffer the token until it's valid UTF-8, then call the callback.
                if let Some(tokens) =
                    token_utf8_buf.push(model.vocabulary().token(*token_id as usize))
                {
                    if let Err(e) = callback(&tokens) {
                        return Err(InferenceError::UserCallback(Box::new(e)));
                    }
                }
            }
        }

        let mut stats = InferenceStats::default();
        let start_at = std::time::SystemTime::now();

        let parameters = request.parameters.unwrap_or(model.inference_parameters());

        // Feed the initial prompt through the transformer, to update its
        // context window with new data.
        self.feed_prompt(
            model,
            parameters,
            request.prompt,
            output_request,
            TokenUtf8Buffer::adapt_callback(&mut callback),
        )?;
        stats.feed_prompt_duration = start_at.elapsed().unwrap();
        stats.prompt_tokens = self.n_past;

        // After the prompt is consumed, sample tokens by repeatedly calling
        // `infer_next_token`. We generate tokens until the model returns an
        // EndOfText token, or we run out of space in the context window,
        // or we reach the specified limit.
        let mut tokens_processed = 0;
        let mut token_utf8_buf = TokenUtf8Buffer::new();
        while tokens_processed < maximum_token_count {
            let token = match self.infer_next_token(model, parameters, &mut Default::default(), rng)
            {
                Ok(token) => token,
                Err(InferenceError::EndOfText) => break,
                Err(e) => return Err(e),
            };

            // Buffer the token until it's valid UTF-8, then call the callback.
            if let Some(tokens) = token_utf8_buf.push(token) {
                if let Err(e) = callback(&tokens) {
                    return Err(InferenceError::UserCallback(Box::new(e)));
                }
            }

            tokens_processed += 1;
        }
        stats.predict_duration = start_at.elapsed().unwrap();
        stats.predict_tokens = self.n_past;

        Ok(stats)
    }

    /// Sample a token using Top-P/Top-K sampling and the last logits from this session.
    pub fn sample_top_p_top_k(
        &self,
        params: &InferenceParameters,
        rng: &mut impl rand::Rng,
    ) -> TokenId {
        let logits = &self.last_logits;
        let n_logits = logits.len();
        let mut logits_id = Vec::<(f32, TokenId)>::with_capacity(n_logits);

        {
            let scale = 1.0 / params.temperature;
            for (i, &logit) in logits.iter().enumerate() {
                let tid = i as TokenId;

                let val = if let Some(logit_override) = params.bias_tokens.get(tid) {
                    logit_override
                } else if self.tokens[self
                    .tokens
                    .len()
                    .saturating_sub(params.repetition_penalty_last_n)..]
                    .contains(&(i as TokenId))
                {
                    // repetition penalty from CTRL paper (https://arxiv.org/abs/1909.05858)
                    // credit https://github.com/facebookresearch/llama/compare/main...shawwn:llama:main

                    // if score < 0 then repetition penalty has to multiplied to reduce the previous token probability
                    if logits[i] < 0.0 {
                        logit * scale * params.repeat_penalty
                    } else {
                        logit * scale / params.repeat_penalty
                    }
                } else {
                    logit * scale
                };
                logits_id.push((val, tid));
            }
        }

        // find the top K tokens
        {
            logits_id.partial_sort(params.top_k, |a, b| {
                // Sort descending
                b.0.total_cmp(&a.0)
            });
            logits_id.truncate(params.top_k);
        }

        let maxl = logits_id
            .iter()
            .map(|x| x.0)
            .max_by(f32::total_cmp)
            .unwrap();

        // compute probs for the top K tokens
        let mut probs: Vec<f32> = logits_id
            .iter()
            .copied()
            .map(|(k, _)| (k - maxl).exp())
            .collect();
        let sum: f32 = probs.iter().copied().sum();

        // Normalize the probs
        for p in probs.iter_mut() {
            *p /= sum;
        }

        // Top p sampling
        if params.top_p < 1.0 {
            let mut cumsum = 0.0;
            for i in 0..probs.len() {
                cumsum += probs[i];
                if cumsum >= params.top_p {
                    probs.truncate(i + 1);
                    logits_id.truncate(i + 1);
                    break;
                }
            }

            cumsum = 1.0 / cumsum;
            for p in probs.iter_mut() {
                *p *= cumsum;
            }
        }

        let dist = WeightedIndex::new(&probs).expect("WeightedIndex error");
        let idx = dist.sample(rng);

        logits_id[idx].1
    }

    /// Obtains a serializable snapshot of the current inference status. This
    /// can be used to cache the state of the model and store them into a file.
    ///
    /// # Safety
    ///
    /// This function provides raw access to the underlying memory owned by the
    /// ggml context. While the provided `InferenceSnapshotRef` object is alive,
    /// no other methods for this model object should be called.
    pub unsafe fn get_snapshot(&mut self) -> InferenceSnapshotRef<'_> {
        let memory_k = unsafe {
            std::slice::from_raw_parts(self.memory_k.data() as *mut u8, self.memory_k.nbytes())
        };
        let memory_v = unsafe {
            std::slice::from_raw_parts(self.memory_v.data() as *mut u8, self.memory_v.nbytes())
        };
        let state = unsafe {
            std::slice::from_raw_parts(self.state.data() as *mut u8, self.state.nbytes())
        };

        InferenceSnapshotRef {
            npast: self.n_past,
            config: self.config,
            tokens: self.tokens.clone(),
            logits: self.last_logits.clone(),
            memory_k,
            memory_v,
            state,
        }
    }

    /// Creates an [InferenceSession] from a snapshot.
    pub fn from_snapshot(
        snapshot: InferenceSnapshot,
        model: &dyn Model,
    ) -> Result<Self, SnapshotError> {
        let mut session = model.start_session(snapshot.config);

        if session.memory_k.nbytes() != snapshot.memory_k.len()
            || session.memory_v.nbytes() != snapshot.memory_v.len()
        {
            return Err(SnapshotError::MemorySizeMismatch {
                self_size: session.memory_k.nbytes() + session.memory_v.nbytes(),
                input_size: snapshot.memory_k.len() + snapshot.memory_v.len(),
            });
        } else if session.state.nbytes() != snapshot.state.len() {
            return Err(SnapshotError::MemorySizeMismatch {
                self_size: session.state.nbytes(),
                input_size: snapshot.state.len(),
            });
        }

        // SAFETY: We have exclusive access to Session, which means no one else
        // should be touching the context's memory. We can write to it because
        // we already checked the size.
        unsafe {
            session.memory_k.write_data(&snapshot.memory_k);
            session.memory_v.write_data(&snapshot.memory_v);
            session.state.write_data(&snapshot.state);
        }

        session.n_past = snapshot.npast;
        session.tokens = snapshot.tokens;
        session.last_logits = snapshot.last_logits;

        Ok(session)
    }
}
impl InferenceSession {
    /// Create a new InferenceSession
    pub fn new(
        config: InferenceSessionConfig,
        n_ctx: usize,
        n_layer: usize,
        n_embd: usize,
        n_vocab: usize,
    ) -> InferenceSession {
        let ctx_size = {
            let mut ctx_size = 0;
            ctx_size += mulf!(
                n_ctx,
                n_layer,
                n_embd,
                ggml::type_sizef(config.memory_k_type.into())
            ); // memory_k
            ctx_size += mulf!(
                n_ctx,
                n_layer,
                n_embd,
                ggml::type_sizef(config.memory_v_type.into())
            ); // memory_v
            ctx_size += (5 + 10 * n_layer) * 256; // object overhead
            ctx_size
        };

        let session_ctx = ggml::Context::init(ctx_size, true);

        // Initialize key + value memory tensors
        let n_mem = n_layer * n_ctx;
        let n_elements = n_embd * n_mem;
        let memory_k = session_ctx.new_tensor_1d(config.memory_k_type.into(), n_elements);
        let memory_v = session_ctx.new_tensor_1d(config.memory_v_type.into(), n_elements);
        let state = session_ctx.new_tensor_1d(ggml::Type::F32, n_layer * 5 * n_embd);

        //  ggml_set_f32(ctx->state, 0.0F);

        for i in 0..n_layer {
            /*
             ggml_set_f32(
                ggml_view_1d(ctx->ctx, ctx->state, n_embed, (5 * i + 4) * n_embed * sizeof(float)),
                -1e30F
            );
            */
        }

        InferenceSession {
            _session_ctx: session_ctx,
            memory_size: ctx_size,
            config,
            memory_k,
            memory_v,
            state,
            n_past: 0,
            mem_per_token: 0,
            tokens: vec![],
            last_logits: vec![0.0; n_vocab],
            scratch: scratch_buffers(),
        }
    }
}
impl Clone for InferenceSession {
    fn clone(&self) -> Self {
        let context = ggml::Context::init(self.memory_size, true);
        let memory_k = context.new_tensor_1d(self.memory_k.get_type(), self.memory_k.nelements());
        let memory_v = context.new_tensor_1d(self.memory_v.get_type(), self.memory_v.nelements());
        let state = context.new_tensor_1d(self.state.get_type(), self.state.nelements());

        Self {
            _session_ctx: context,
            memory_size: self.memory_size,
            config: self.config,
            memory_k,
            memory_v,
            state,
            n_past: self.n_past,
            mem_per_token: self.mem_per_token,
            tokens: self.tokens.clone(),
            last_logits: self.last_logits.clone(),
            scratch: scratch_buffers(),
        }
    }
}

#[derive(Error, Debug)]
/// Errors encountered during the snapshot process.
pub enum SnapshotError {
    /// Arbitrary I/O error.
    #[error("I/O error while reading or writing snapshot")]
    IO(#[from] std::io::Error),
    /// Mismatch between the snapshotted memory and the in-memory memory.
    #[error("could not read snapshot due to size mismatch (self={self_size}, input={input_size})")]
    MemorySizeMismatch {
        /// The size of the session memory in memory.
        self_size: usize,
        /// The size of the session memory in snapshot.
        input_size: usize,
    },
}

#[derive(serde::Serialize, Clone, PartialEq)]
/// A serializable snapshot of the inference process.
/// Can be created by calling [InferenceSession::get_snapshot].
///
/// If serializing, ensure that your serializer is binary-efficient.
/// This type contains a large array of bytes; traditional textual serializers
/// are likely to serialize this as an array of numbers at extreme cost.
// Keep in sync with [InferenceSession] and [InferenceSnapshot].
pub struct InferenceSnapshotRef<'a> {
    /// How many tokens have been stored in the memory so far.
    pub npast: usize,
    /// Parameters associated with the saved inference session.
    pub config: InferenceSessionConfig,
    /// All tokens generated by this inference session.
    pub tokens: Vec<TokenId>,
    /// The vector of logits that was produced after the last inference.
    pub logits: Vec<f32>,
    /// The contents of the 'key' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_k: &'a [u8],
    /// The contents of the 'value' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_v: &'a [u8],
    /// The contents of the 'state' memory tensor.
    #[serde(with = "serde_bytes")]
    pub state: &'a [u8],
}
impl InferenceSnapshotRef<'_> {
    /// Creates an owned [InferenceSnapshot] from this [InferenceSnapshotRef].
    ///
    /// The [ToOwned] trait is not used due to its blanket implementation for all [Clone] types.
    pub fn to_owned(&self) -> InferenceSnapshot {
        InferenceSnapshot {
            npast: self.npast,
            config: self.config,
            tokens: self.tokens.clone(),
            last_logits: self.logits.clone(),
            memory_k: self.memory_k.to_vec(),
            memory_v: self.memory_v.to_vec(),
            state: self.state.to_vec(),
        }
    }
}

/// A serializable snapshot of the inference process. Can be restored by calling
/// [InferenceSession::from_snapshot].
#[derive(serde::Deserialize, Clone, PartialEq)]
// Keep in sync with [InferenceSession] and [InferenceSnapshotRef].
pub struct InferenceSnapshot {
    /// How many tokens have been stored in the memory so far.
    pub npast: usize,
    /// Parameters associated with the saved inference session.
    pub config: InferenceSessionConfig,
    /// All tokens generated by this inference session.
    pub tokens: Vec<TokenId>,
    /// The vector of logits that was produced after the last inference.
    pub last_logits: Vec<f32>,
    /// The contents of the 'key' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_k: Vec<u8>,
    /// The contents of the 'value' memory tensor.
    #[serde(with = "serde_bytes")]
    pub memory_v: Vec<u8>,
    /// The contents of the 'state' memory tensor.
    #[serde(with = "serde_bytes")]
    pub state: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
/// Configuration for an inference session.
///
/// This is specified at the time of creation of an [InferenceSession],
/// and cannot be changed after the session has been created.
pub struct InferenceSessionConfig {
    /// The type of the memory K tensor.
    pub memory_k_type: ModelKVMemoryType,
    /// The type of the memory V tensor.
    pub memory_v_type: ModelKVMemoryType,
}
impl Default for InferenceSessionConfig {
    fn default() -> Self {
        Self {
            memory_k_type: ModelKVMemoryType::Float32,
            memory_v_type: ModelKVMemoryType::Float32,
        }
    }
}

#[derive(Debug, PartialEq, Default, Clone, Copy)]
/// Settings specific to [InferenceSession::infer].
pub struct InferenceRequest<'a> {
    /// The prompt to feed to the model.
    pub prompt: &'a str,
    /// The parameters to use during this inference attempt.
    /// If not specified, this will default to the parameters
    /// specified in the model.
    pub parameters: Option<&'a InferenceParameters>,
    /// Whether or not to call the callback with the previous tokens
    /// that were encountered in this session.
    ///
    /// You likely want to turn this on if you're using a session
    /// that has been rehydrated from a snapshot.
    pub play_back_previous_tokens: bool,
    /// The maximum number of tokens to generate.
    pub maximum_token_count: Option<usize>,
}

/// Statistics about the inference process.
#[derive(Debug, Clone, Copy)]
pub struct InferenceStats {
    /// How long it took to feed the prompt.
    pub feed_prompt_duration: std::time::Duration,
    /// How many tokens the prompt was.
    pub prompt_tokens: usize,
    /// How long it took to predict new tokens.
    pub predict_duration: std::time::Duration,
    /// The number of predicted tokens.
    pub predict_tokens: usize,
}
impl Default for InferenceStats {
    fn default() -> Self {
        Self {
            feed_prompt_duration: std::time::Duration::from_secs(0),
            prompt_tokens: 0,
            predict_duration: std::time::Duration::from_secs(0),
            predict_tokens: 0,
        }
    }
}
impl Display for InferenceStats {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "feed_prompt_duration: {}ms\nprompt_tokens: {}\npredict_duration: {}ms\npredict_tokens: {}\nper_token_duration: {:.3}ms",
            self.feed_prompt_duration.as_millis(),
            self.prompt_tokens,
            self.predict_duration.as_millis(),
            self.predict_tokens,
            (self.predict_duration.as_millis() as f64) / (self.predict_tokens as f64),
        )
    }
}

/// Allowed types for the model memory K/V tensors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModelKVMemoryType {
    /// 16-bit float.
    Float16,
    /// 32-bit float.
    Float32,
}
impl From<ModelKVMemoryType> for ggml::Type {
    fn from(value: ModelKVMemoryType) -> Self {
        match value {
            ModelKVMemoryType::Float16 => ggml::Type::F16,
            ModelKVMemoryType::Float32 => ggml::Type::F32,
        }
    }
}

fn scratch_buffers() -> [ggml::Buffer; 2] {
    [
        ggml::Buffer::new(SCRATCH_SIZE),
        ggml::Buffer::new(SCRATCH_SIZE),
    ]
}
