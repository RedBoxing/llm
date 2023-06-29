//! An implementation of the [RWKV](https://github.com/BlinkDL/RWKV-LM) model for the `llm` ecosystem.
//!
//! This implementation is out of date and needs to be updated to the latest version of the model
//! architecture.
//!
//! This model will not be generally available in the `llm` ecosystem until it is updated.
//! It is currently only available for testing purposes.

// Ref: https://github.com/saharNooby/rwkv.cpp/blob/5eb8f09/rwkv.cpp

use std::cell::RefCell;

use ggml::Tensor;
use llm_base::{
    ggml, model::common, model::HyperparametersWriteError, util, FileType, InferenceParameters,
    InferenceSession, InferenceSessionConfig, KnownModel, LoadError, Mmap, ModelParameters,
    OutputRequest, TokenId, Vocabulary,
};

pub struct Rwkv {
    hyperparameters: Hyperparameters,
    context_size: usize,

    vocabulary: Vocabulary,

    // Must be kept alive for the model
    context: ggml::Context,

    // Must be kept alive to evaluate the graph
    ctx: ggml::Context,

    graph: RefCell<ggml::ComputationGraph>,
    token_index: ggml::Tensor,
    state: RefCell<ggml::Tensor>,
    logits: RefCell<ggml::Tensor>,
    state_parts: RefCell<Vec<ggml::Tensor>>,
}
unsafe impl Send for Rwkv {}
unsafe impl Sync for Rwkv {}

fn rwkv_layer_norm(
    ctx: &ggml::Context,
    x: &ggml::Tensor,
    weight: &ggml::Tensor,
    bias: &ggml::Tensor,
) -> ggml::Tensor {
    let x = ctx.op_norm(x);
    let x = ctx.op_mul(&x, weight);
    ctx.op_add(&x, bias)
}

extern "C" fn rwkv_exp_impl(n_cols: i32, dest: *mut f32, src: *const f32) {
    for i in 0..n_cols {
        unsafe {
            *dest.add(i as usize) = (*src.add(i as usize)).exp();
        }
    }
}

extern "C" fn rwkv_sigmoid_impl(n_cols: i32, dest: *mut f32, src: *const f32) {
    for i in 0..n_cols {
        unsafe {
            *dest.add(i as usize) = 1.0 / (1.0 + (-(*src.add(i as usize))).exp());
        }
    }
}

extern "C" fn rwkv_max_impl(n_cols: i32, dest: *mut f32, src0: *const f32, src1: *const f32) {
    for i in 0..n_cols {
        unsafe {
            *dest.add(i as usize) = (*src0.add(i as usize)).max(*src1.add(i as usize));
        }
    }
}

extern "C" fn rwkv_1_minus_x_impl(n_cols: i32, dest: *mut f32, src: *const f32) {
    for i in 0..n_cols {
        unsafe {
            *dest.add(i as usize) = 1.0 - *src.add(i as usize);
        }
    }
}

fn rwkv_exp(ctx: &ggml::Context, x: &ggml::Tensor) -> ggml::Tensor {
    unsafe { ctx.op_map_unary(x, rwkv_exp_impl) }
}

fn rwkv_sigmoid(ctx: &ggml::Context, x: &ggml::Tensor) -> ggml::Tensor {
    unsafe { ctx.op_map_unary(x, rwkv_sigmoid_impl) }
}

fn rwkv_max(ctx: &ggml::Context, x: &ggml::Tensor, y: &ggml::Tensor) -> ggml::Tensor {
    unsafe { ctx.op_map_binary(x, y, rwkv_max_impl) }
}

fn rwkv_1_minus_x(ctx: &ggml::Context, x: &ggml::Tensor) -> ggml::Tensor {
    unsafe { ctx.op_map_unary(x, rwkv_1_minus_x_impl) }
}

impl KnownModel for Rwkv {
    type Hyperparameters = Hyperparameters;

    fn new<E: std::error::Error>(
        hyperparameters: Self::Hyperparameters,
        params: ModelParameters,
        vocabulary: Vocabulary,
        tensor_loader: impl llm_base::TensorLoader<E>,
    ) -> Result<Self, E>
    where
        Self: Sized,
    {
        let n_layer = hyperparameters.n_layer;
        let mut tl = tensor_loader;

        // prepare memory for weights
        let emb = tl.load("emb.weight")?;
        let ln0_weight = tl.load("blocks.0.ln0.weight")?;
        let ln0_bias = tl.load("blocks.0.ln0.bias")?;

        let mut layers = Vec::new();
        for i in 0..n_layer {
            let layer = Layer {
                ln1_weight: tl.load(&format!("blocks.{i}.ln1.weight"))?,
                ln1_bias: tl.load(&format!("blocks.{i}.ln1.bias"))?,
                att_time_mix_k: tl.load(&format!("blocks.{i}.att.time_mix_k"))?,
                att_time_mix_v: tl.load(&format!("blocks.{i}.att.time_mix_v"))?,
                att_time_mix_r: tl.load(&format!("blocks.{i}.att.time_mix_r"))?,
                att_time_first: tl.load(&format!("blocks.{i}.att.time_first"))?,
                att_time_decay: tl.load(&format!("blocks.{i}.att.time_decay"))?,
                att_key: tl.load(&format!("blocks.{i}.att.key.weight"))?,
                att_value: tl.load(&format!("blocks.{i}.att.value.weight"))?,
                att_receptance: tl.load(&format!("blocks.{i}.att.receptance.weight"))?,
                att_output: tl.load(&format!("blocks.{i}.att.output.weight"))?,
                ln2_weight: tl.load(&format!("blocks.{i}.ln2.weight"))?,
                ln2_bias: tl.load(&format!("blocks.{i}.ln2.bias"))?,
                ffn_time_mix_k: tl.load(&format!("blocks.{i}.ffn.time_mix_k"))?,
                ffn_time_mix_r: tl.load(&format!("blocks.{i}.ffn.time_mix_r"))?,
                ffn_key: tl.load(&format!("blocks.{i}.ffn.key.weight"))?,
                ffn_value: tl.load(&format!("blocks.{i}.ffn.value.weight"))?,
                ffn_receptance: tl.load(&format!("blocks.{i}.ffn.receptance.weight"))?,
            };

            layers.push(layer);
        }

        let ln_out_weight = tl.load("ln_out.weight")?;
        let ln_out_bias = tl.load("ln_out.bias")?;
        let head = tl.load("head.weight")?;

        let (context, _) = tl.finish();

        let ModelParameters { context_size, .. } = params;

        let Hyperparameters {
            n_vocab,
            n_embd,
            n_layer,
            file_type: _,
        } = hyperparameters;

        let ctx0 = ggml::Context::init(
            100 * n_embd * 4 + 2 * 5 * n_layer * n_embd * 4 + n_vocab * 4 + 256 * 1024 * 1024,
            true,
        );

        let state = ctx0.new_tensor_1d(ggml::Type::F32, n_layer * 5 * n_embd);

        let token_index = ctx0.new_tensor_1d(ggml::Type::I32, 1);
        let mut x = ctx0.op_get_rows(&emb, &token_index);

        x = rwkv_layer_norm(&ctx0, &x, &ln0_weight, &ln0_bias);

        let mut state_parts: Vec<ggml::Tensor> = Vec::with_capacity(n_layer * 5);

        for _ in 0..n_layer * 5 {
            let part = ctx0.new_tensor_1d(ggml::Type::F32, 0);
            state_parts.push(part);
        }

        for i in 0..n_layer {
            let layer = &layers[i];

            // RWKV Time Mixing
            {
                let x0 = rwkv_layer_norm(&ctx0, &x, &layer.ln1_weight, &layer.ln1_bias);
                let x_prev = ctx0.op_view_1d(&state, n_embd, (5 * i + 1) * n_embd * 4);

                let xk = ctx0.op_add(
                    &ctx0.op_mul(&x0, &layer.att_time_mix_k),
                    &ctx0.op_mul(&x_prev, &rwkv_1_minus_x(&ctx0, &layer.att_time_mix_k)),
                );

                let xv = ctx0.op_add(
                    &ctx0.op_mul(&x0, &layer.att_time_mix_v),
                    &ctx0.op_mul(&x_prev, &rwkv_1_minus_x(&ctx0, &layer.att_time_mix_v)),
                );

                let xr = ctx0.op_add(
                    &ctx0.op_mul(&x0, &layer.att_time_mix_r),
                    &ctx0.op_mul(&x_prev, &rwkv_1_minus_x(&ctx0, &layer.att_time_mix_r)),
                );

                state_parts[5 * i + 1] = x0;

                let r = rwkv_sigmoid(&ctx0, &ctx0.op_mul_mat(&layer.att_receptance, &xr));
                let k = ctx0.op_mul_mat(&layer.att_key, &xk);
                let v = ctx0.op_mul_mat(&layer.att_value, &xv);

                let aa = ctx0.op_view_1d(&state, n_embd, (5 * i + 2) * n_embd * 4);
                let bb = ctx0.op_view_1d(&state, n_embd, (5 * i + 3) * n_embd * 4);
                let pp = ctx0.op_view_1d(&state, n_embd, (5 * i + 4) * n_embd * 4);

                let mut ww = ctx0.op_add(&layer.att_time_first, &k);
                let mut qq = rwkv_max(&ctx0, &pp, &ww);

                let mut e1 = rwkv_exp(&ctx0, &ctx0.op_sub(&pp, &qq));
                let mut e2 = rwkv_exp(&ctx0, &ctx0.op_sub(&ww, &qq));

                let a = ctx0.op_add(&ctx0.op_mul(&e1, &aa), &ctx0.op_mul(&e2, &v));
                let b = ctx0.op_add(&ctx0.op_mul(&e1, &bb), &e2);

                let wkv = ctx0.op_div(&a, &b);

                ww = ctx0.op_add(&pp, &layer.att_time_decay);
                qq = rwkv_max(&ctx0, &ww, &k);
                e1 = rwkv_exp(&ctx0, &ctx0.op_sub(&ww, &qq));
                e2 = rwkv_exp(&ctx0, &ctx0.op_sub(&k, &qq));

                state_parts[5 * i + 2] = ctx0.op_add(&ctx0.op_mul(&e1, &aa), &ctx0.op_mul(&e2, &v));
                state_parts[5 * i + 3] = ctx0.op_add(&ctx0.op_mul(&e1, &bb), &e2);
                state_parts[5 * i + 4] = qq;

                x = ctx0.op_add(
                    &x,
                    &ctx0.op_mul_mat(&layer.att_output, &ctx0.op_mul(&r, &wkv)),
                );
            }

            // RWKV FNN/channel mixing

            #[allow(clippy::identity_op)]
            {
                let x0 = rwkv_layer_norm(&ctx0, &x, &layer.ln2_weight, &layer.ln2_bias);
                let x_prev = ctx0.op_view_1d(&state, n_embd, (5 * i + 0) * n_embd * 4);

                let xk = ctx0.op_add(
                    &ctx0.op_mul(&x0, &layer.ffn_time_mix_k),
                    &ctx0.op_mul(&x_prev, &rwkv_1_minus_x(&ctx0, &layer.ffn_time_mix_k)),
                );
                let xr = ctx0.op_add(
                    &ctx0.op_mul(&x0, &layer.ffn_time_mix_r),
                    &ctx0.op_mul(&x_prev, &rwkv_1_minus_x(&ctx0, &layer.ffn_time_mix_r)),
                );

                state_parts[5 * i + 0] = x0;

                let r = rwkv_sigmoid(&ctx0, &ctx0.op_mul_mat(&layer.ffn_receptance, &xr));
                let k = ctx0.op_sqrt(&ctx0.op_relu(&ctx0.op_mul_mat(&layer.ffn_key, &xk)));

                x = ctx0.op_add(&x, &ctx0.op_mul(&r, &ctx0.op_mul_mat(&layer.ffn_value, &k)));
            }
        }

        x = rwkv_layer_norm(&ctx0, &x, &ln_out_weight, &ln_out_bias);

        let logits = ctx0.op_mul_mat(&head, &x);

        let mut gf = ggml::ComputationGraph::new(1);
        gf.build_forward_expand(&logits);

        for part in state_parts.iter().take(n_layer * 5) {
            gf.build_forward_expand(part);
        }

        Ok(Rwkv {
            hyperparameters,
            context_size,
            vocabulary,
            context,
            ctx: ctx0,
            graph: RefCell::new(gf),
            token_index,
            state: RefCell::new(state),
            logits: RefCell::new(logits),
            state_parts: RefCell::new(state_parts),
        })
    }

    fn start_session(&self, params: InferenceSessionConfig) -> InferenceSession {
        InferenceSession::new(
            params,
            self.context_size,
            self.hyperparameters.n_layer,
            self.hyperparameters.n_embd,
            self.hyperparameters.n_vocab,
            true,
        )
    }

    fn evaluate(
        &self,
        session: &mut InferenceSession,
        _params: &InferenceParameters,
        input_tokens: &[TokenId],
        output_request: &mut OutputRequest,
    ) {
        /*
        for token in input_tokens {
            self.token_index.set_i32_1d(0, (*token) as i32);

            unsafe {
                /*self.state
                .borrow_mut()
                .write_data(std::slice::from_raw_parts(
                    session.state.data() as *const u8,
                    session.state.nbytes(),
                ));*/

                //self.state.borrow_mut().set_data(session.state.data());

                std::ptr::copy_nonoverlapping(
                    session.state.data() as *mut f32,
                    self.state.borrow_mut().data() as *mut f32,
                    session.state.get_ne()[0] as usize,
                );

                // idk wich one I should use they give me different results
            }

            self.ctx.graph_compute(&mut self.graph.borrow_mut());

            for i in 0..(self.hyperparameters.n_layer * 5) {
                let part = &mut self.state_parts.borrow_mut()[i];
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        part.data() as *mut f32,
                        (session.state.data() as *mut f32).add(i * self.hyperparameters.n_embd),
                        part.get_ne()[0] as usize,
                    );
                }
            }

            assert_eq!(session.last_logits.len(), self.hyperparameters.n_vocab);
            unsafe {
                //self.logits.read_data(0, bytemuck::cast_slice_mut(&mut session.last_logits));
                std::ptr::copy_nonoverlapping(
                    self.logits.borrow_mut().data() as *mut f32,
                    session.last_logits.as_mut_slice().as_ptr() as *mut f32,
                    self.hyperparameters.n_vocab,
                );
            }

            output_request.all_logits = Some(session.last_logits.clone());

            common::extract_embeddings(
                output_request,
                &self.token_index,
                self.hyperparameters.n_embd,
                input_tokens.len(),
            );
            common::update_session(session, &self.ctx, input_tokens.len(), input_tokens.len());
        }
        */
    }

    fn vocabulary(&self) -> &Vocabulary {
        &self.vocabulary
    }

    fn context_size(&self) -> usize {
        self.context_size
    }

    fn bot_token_id(&self) -> Option<TokenId> {
        None
    }

    fn eot_token_id(&self) -> TokenId {
        self.vocabulary.id("<|endoftext|>".as_bytes()).unwrap() as TokenId
    }

    fn quantize_tensors() -> Vec<llm_base::Regex> {
        vec![]
    }

    fn skip_quantize_tensors() -> Vec<llm_base::Regex> {
        vec![]
    }
}

/// The hyperparameters of the model.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct Hyperparameters {
    /// n_vocab
    pub n_vocab: usize,
    /// n_embd
    pub n_embd: usize,
    /// n_layer
    pub n_layer: usize,
    /// file_type
    pub file_type: FileType,
}

impl llm_base::Hyperparameters for Hyperparameters {
    fn read_ggml(reader: &mut dyn std::io::BufRead) -> Result<Self, LoadError> {
        Ok(Hyperparameters {
            n_vocab: util::read_i32(reader)?.try_into()?,
            n_embd: util::read_i32(reader)?.try_into()?,
            n_layer: util::read_i32(reader)?.try_into()?,
            file_type: util::read_filetype(reader)?,
        })
    }

    fn write_ggml(&self, writer: &mut dyn std::io::Write) -> Result<(), HyperparametersWriteError> {
        util::write_i32(writer, self.n_vocab.try_into()?)?;
        util::write_i32(writer, self.n_embd.try_into()?)?;
        util::write_i32(writer, self.n_layer.try_into()?)?;
        util::write_i32(writer, self.file_type.into())?;
        Ok(())
    }

    fn n_vocabulary(&self) -> usize {
        self.n_vocab
    }

    fn file_type(&self) -> Option<FileType> {
        Some(self.file_type)
    }

    fn file_type_mut(&mut self) -> Option<&mut FileType> {
        Some(&mut self.file_type)
    }
}

struct Layer {
    ln1_weight: Tensor,
    ln1_bias: Tensor,

    // RWKV, also called "attention" by the author.
    att_time_mix_k: Tensor,
    att_time_mix_v: Tensor,
    att_time_mix_r: Tensor,
    att_time_first: Tensor,
    att_time_decay: Tensor,
    att_key: Tensor,
    att_value: Tensor,
    att_receptance: Tensor,
    att_output: Tensor,

    ln2_weight: Tensor,
    ln2_bias: Tensor,

    // FFN.
    ffn_time_mix_k: Tensor,
    ffn_time_mix_r: Tensor,
    ffn_key: Tensor,
    ffn_value: Tensor,
    ffn_receptance: Tensor,
}