//! An implementation of [GPT-2](https://huggingface.co/docs/transformers/model_doc/gpt2) for the `llm` ecosystem.
#![deny(missing_docs)]

use ggml::Tensor;
use llm_base::{
    ggml,
    model::{common, HyperparametersWriteError},
    util, FileType, InferenceParameters, InferenceSession, InferenceSessionConfig, KnownModel,
    LoadError, ModelParameters, OutputRequest, TokenId,
};

use tokenizers::Tokenizer;

/// The GPT-2 model. Ref: [The Illustrated GPT-2](https://jalammar.github.io/illustrated-gpt2/)
///
/// # Safety
/// This implements [Send] and [Sync] as it is immutable after construction.
pub struct Gpt2 {
    hyperparameters: Hyperparameters,
    n_context_tokens: usize,
    tokenizer: Tokenizer,
    ln_f_g: Tensor,
    ln_f_b: Tensor,
    wte: Tensor,
    wpe: Tensor,
    lm_head: Tensor,
    layers: Vec<Layer>,
    inference_params: InferenceParameters,
    _context: ggml::Context,
    _mmap: Option<llm_base::Mmap>,
}
unsafe impl Send for Gpt2 {}
unsafe impl Sync for Gpt2 {}

impl KnownModel for Gpt2 {
    type Hyperparameters = Hyperparameters;
    type Overrides = ();

    fn new<E: std::error::Error>(
        hyperparameters: Self::Hyperparameters,
        params: ModelParameters,
        tokenizer: Tokenizer,
        _overrides: Option<Self::Overrides>,
        tensor_loader: impl llm_base::TensorLoader<E>,
    ) -> Result<Self, E> {
        let mut tl = tensor_loader;
        // prepare memory for weights
        let ln_f_g = tl.load("model/ln_f/g")?;
        let ln_f_b = tl.load("model/ln_f/b")?;
        let wte = tl.load("model/wte")?;
        let wpe = tl.load("model/wpe")?;
        let lm_head = tl.load("model/lm_head")?;

        let mut layers = Vec::new();
        for i in 0..hyperparameters.n_layer {
            let layer = Layer {
                ln_1_g: tl.load(&format!("model/h{i}/ln_1/g"))?,
                ln_1_b: tl.load(&format!("model/h{i}/ln_1/b"))?,
                ln_2_g: tl.load(&format!("model/h{i}/ln_2/g"))?,
                ln_2_b: tl.load(&format!("model/h{i}/ln_2/b"))?,
                c_attn_attn_w: tl.load(&format!("model/h{i}/attn/c_attn/w"))?,
                c_attn_attn_b: tl.load(&format!("model/h{i}/attn/c_attn/b"))?,
                c_attn_proj_w: tl.load(&format!("model/h{i}/attn/c_proj/w"))?,
                c_attn_proj_b: tl.load(&format!("model/h{i}/attn/c_proj/b"))?,
                c_mlp_fc_w: tl.load(&format!("model/h{i}/mlp/c_fc/w"))?,
                c_mlp_fc_b: tl.load(&format!("model/h{i}/mlp/c_fc/b"))?,
                c_mlp_proj_w: tl.load(&format!("model/h{i}/mlp/c_proj/w"))?,
                c_mlp_proj_b: tl.load(&format!("model/h{i}/mlp/c_proj/b"))?,
            };

            layers.push(layer);
        }

        let (_context, _, _mmap) = tl.finish();

        let ModelParameters {
            n_context_tokens,
            inference_parameters: inference_params,
            ..
        } = params;

        Ok(Gpt2 {
            hyperparameters,
            n_context_tokens,
            tokenizer,
            layers,
            ln_f_g,
            ln_f_b,
            wte,
            wpe,
            lm_head,
            inference_params,
            _context,
            _mmap,
        })
    }

    fn start_session(&self, config: InferenceSessionConfig) -> InferenceSession {
        InferenceSession::new(
            config,
            self.hyperparameters.n_ctx,
            self.hyperparameters.n_layer,
            self.hyperparameters.n_embd,
            self.hyperparameters.n_vocab,
            false,
        )
    }

    fn evaluate(
        &self,
        session: &mut InferenceSession,
        params: &InferenceParameters,
        input_tokens: &[TokenId],
        output_request: &mut OutputRequest,
    ) {
        let n = input_tokens.len();
        let n_threads = params.n_threads;

        let Hyperparameters {
            n_embd,
            n_head,
            n_vocab,
            n_layer,
            ..
        } = self.hyperparameters;
        let n_ctx = self.n_context_tokens;

        let (ctx0, embd) = common::prepare_for_evaluate(n_layer, session, input_tokens);

        let n_past = session.n_past;

        let mut position_buf = vec![];
        for position_idx in 0..n {
            position_buf.push(n_past + position_idx);
        }

        let mut position = ctx0.new_tensor_1d(ggml::Type::I32, n);
        unsafe { position.write_data(bytemuck::cast_slice(&position_buf)) };

        let mut input_layer = ctx0.op_add(
            &ctx0.op_get_rows(&self.wte, &embd),
            &ctx0.op_get_rows(&self.wpe, &position),
        );

        let memory_k = &session.memory_k;
        let memory_k_size = memory_k.element_size();

        let memory_v = &session.memory_v;
        let memory_v_size = memory_v.element_size();

        let mut gf = ggml::ComputationGraph::new(n_threads);

        for il in 0..n_layer {
            // norm
            let mut current = ctx0.op_norm(&input_layer);
            current = ctx0.op_add(
                &ctx0.op_mul(&ctx0.op_repeat(&self.layers[il].ln_1_g, &current), &current),
                &ctx0.op_repeat(&self.layers[il].ln_1_b, &current),
            );

            // attn
            current = ctx0.op_mul_mat(&self.layers[il].c_attn_attn_w, &current);
            current = ctx0.op_add(
                &ctx0.op_repeat(&self.layers[il].c_attn_attn_b, &current),
                &current,
            );

            // self-attn
            let nb = current.get_nb()[1];
            let f32_size = std::mem::size_of::<f32>();
            let qcur = ctx0.op_view_2d(&current, (n_embd, n), nb, 0);
            let kcur = ctx0.op_view_2d(&current, (n_embd, n), nb, f32_size * n_embd);
            let vcur = ctx0.op_view_2d(&current, (n_embd, n), nb, f32_size * n_embd * 2);

            if n >= 1 {
                let k = ctx0.op_view_1d(
                    memory_k,
                    n * n_embd,
                    (memory_k_size * n_embd) * (il * n_ctx + n_past),
                );
                let v = ctx0.op_view_1d(
                    memory_v,
                    n * n_embd,
                    (memory_v_size * n_embd) * (il * n_ctx + n_past),
                );

                gf.build_forward_expand(&ctx0.op_cpy(&kcur, &k));
                gf.build_forward_expand(&ctx0.op_cpy(&vcur, &v));
            }

            let q = ctx0.op_permute(
                &ctx0.op_cpy(
                    &qcur,
                    &ctx0.new_tensor_3d(ggml::Type::F32, n_embd / n_head, n_head, n),
                ),
                0,
                2,
                1,
                3,
            );

            let k = ctx0.op_permute(
                &ctx0.op_reshape_3d(
                    &ctx0.op_view_1d(
                        &session.memory_k,
                        (n_past + n) * n_embd,
                        il * n_ctx * memory_k_size * n_embd,
                    ),
                    n_embd / n_head,
                    n_head,
                    n_past + n,
                ),
                0,
                2,
                1,
                3,
            );

            let kq = ctx0.op_mul_mat(&k, &q);
            let kq_scaled = ctx0.op_scale_inplace(
                &kq,
                &ctx0.new_f32(1f32 / f32::sqrt(n_embd as f32 / n_head as f32)),
            );

            let kq_masked = ctx0.op_diag_mask_inf_inplace(&kq_scaled, n_past);
            let kq_softmax = ctx0.op_soft_max_inplace(&kq_masked);

            let v_trans = ctx0.op_cpy(
                &ctx0.op_permute(
                    &ctx0.op_reshape_3d(
                        &ctx0.op_view_1d(
                            memory_v,
                            (n_past + n) * n_embd,
                            il * n_ctx * memory_v_size * n_embd,
                        ),
                        n_embd / n_head,
                        n_head,
                        n_past + n,
                    ),
                    1,
                    2,
                    0,
                    3,
                ),
                &ctx0.new_tensor_3d(memory_v.get_type(), n_past + n, n_embd / n_head, n_head),
            );

            let kqv = ctx0.op_mul_mat(&v_trans, &kq_softmax);
            let kqv_merged = ctx0.op_permute(&kqv, 0, 2, 1, 3);

            current = ctx0.op_cpy(&kqv_merged, &ctx0.new_tensor_2d(ggml::Type::F32, n_embd, n));

            // projection
            current = ctx0.op_mul_mat(&self.layers[il].c_attn_proj_w, &current);
            current = ctx0.op_add(
                &ctx0.op_repeat(&self.layers[il].c_attn_proj_b, &current),
                &current,
            );

            // add input
            current = ctx0.op_add(&current, &input_layer);

            // feed-forward
            let ff_in = current.share();

            // feed-forward normalization
            current = ctx0.op_norm(&ff_in);
            current = ctx0.op_add(
                &ctx0.op_mul(&ctx0.op_repeat(&self.layers[il].ln_2_g, &current), &current),
                &ctx0.op_repeat(&self.layers[il].ln_2_b, &current),
            );

            // feed-forward fully connected
            current = ctx0.op_mul_mat(&self.layers[il].c_mlp_fc_w, &current);
            current = ctx0.op_add(
                &ctx0.op_repeat(&self.layers[il].c_mlp_fc_b, &current),
                &current,
            );

            // feed-forward activation
            current = ctx0.op_gelu(&current);

            // feed-forward projection
            current = ctx0.op_mul_mat(&self.layers[il].c_mlp_proj_w, &current);
            current = ctx0.op_add(
                &ctx0.op_repeat(&self.layers[il].c_mlp_proj_b, &current),
                &current,
            );

            // input for next layer
            input_layer = ctx0.op_add(&current, &ff_in);
        }

        // normalization
        input_layer = ctx0.op_norm(&input_layer);
        input_layer = ctx0.op_add(
            &ctx0.op_mul(&ctx0.op_repeat(&self.ln_f_g, &input_layer), &input_layer),
            &ctx0.op_repeat(&self.ln_f_b, &input_layer),
        );

        input_layer = ctx0.op_mul_mat(&self.lm_head, &input_layer);

        // run the computation
        gf.build_forward_expand(&input_layer);
        ctx0.graph_compute(&mut gf);

        // finish evaluation
        common::read_last_token(session, &input_layer, n_vocab, n);
        common::extract_logits(output_request, &input_layer, n_vocab, n);
        common::extract_embeddings(output_request, &embd, n_embd, n);
        common::update_session(session, &ctx0, input_tokens.len(), n);
    }

    fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }

    fn n_context_tokens(&self) -> usize {
        self.hyperparameters.n_ctx
    }

    fn bot_token_id(&self) -> Option<TokenId> {
        None
    }

    fn eot_token_id(&self) -> TokenId {
        self.tokenizer().token_to_id("<|endoftext|>").unwrap() as i32
    }

    fn inference_parameters(&self) -> &InferenceParameters {
        &self.inference_params
    }
}

/// GPT-2 [hyperparameters](https://en.wikipedia.org/wiki/Hyperparameter_(machine_learning))
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct Hyperparameters {
    /// Size of the model's vocabulary
    n_vocab: usize,
    /// Size of the model's context
    n_ctx: usize,
    /// Size of the model's embedding layer
    n_embd: usize,
    /// n_head
    n_head: usize,
    /// Number of layers in the model
    n_layer: usize,
    /// file type
    file_type: FileType,
}
impl llm_base::Hyperparameters for Hyperparameters {
    fn read_ggml(reader: &mut dyn std::io::BufRead) -> Result<Self, LoadError> {
        let hyperparameters = Hyperparameters {
            n_vocab: util::read_i32(reader)?.try_into()?,
            n_ctx: util::read_i32(reader)?.try_into()?,
            n_embd: util::read_i32(reader)?.try_into()?,
            n_head: util::read_i32(reader)?.try_into()?,
            n_layer: util::read_i32(reader)?.try_into()?,
            file_type: util::read_filetype(reader)?,
        };

        let n_vocab = util::read_i32(reader)? as usize;
        if hyperparameters.n_vocab != n_vocab {
            return Err(LoadError::InvariantBroken {
                path: None,
                invariant: format!(
                    "GPT2 model expected n_vocab {} found {}",
                    hyperparameters.n_vocab, n_vocab
                ),
            });
        }

        Ok(hyperparameters)
    }

    fn write_ggml(&self, writer: &mut dyn std::io::Write) -> Result<(), HyperparametersWriteError> {
        util::write_i32(writer, self.n_vocab.try_into()?)?;
        util::write_i32(writer, self.n_ctx.try_into()?)?;
        util::write_i32(writer, self.n_embd.try_into()?)?;
        util::write_i32(writer, self.n_head.try_into()?)?;
        util::write_i32(writer, self.n_layer.try_into()?)?;
        util::write_i32(writer, self.file_type.into())?;
        util::write_i32(writer, self.n_vocab.try_into()?)?;

        Ok(())
    }

    fn n_vocabulary(&self) -> usize {
        self.n_vocab
    }

    fn file_type(&self) -> Option<FileType> {
        Some(self.file_type)
    }
}

struct Layer {
    // normalization
    ln_1_g: Tensor,
    ln_1_b: Tensor,

    ln_2_g: Tensor,
    ln_2_b: Tensor,

    // attention
    c_attn_attn_w: Tensor,
    c_attn_attn_b: Tensor,

    c_attn_proj_w: Tensor,
    c_attn_proj_b: Tensor,

    // mlp
    c_mlp_fc_w: Tensor,
    c_mlp_fc_b: Tensor,

    c_mlp_proj_w: Tensor,
    c_mlp_proj_b: Tensor,
}
