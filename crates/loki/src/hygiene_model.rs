//! Pure-Rust forward pass of the vendored `codehygiene` 1.58-bit ternary model (`vendor/codehygiene/weights.npz`).
//! Runs in-process with no Python or NumPy subprocess.

use crate::hygiene::Squiggle;
use flate2::read::DeflateDecoder;
use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

const WEIGHTS_NPZ: &[u8] = include_bytes!("../vendor/codehygiene/weights.npz");

const MAX_COLS: usize = 128;
const MAX_LINES: usize = 2048;
const MAX_REL_DIST: isize = 32;
const SKETCH_DIM: usize = 40;
const THRESHOLD: f32 = 0.55;
const POW3: [u8; 5] = [1, 3, 9, 27, 81];

const SPEC: [(&str, &str); 8] = [
    ("deep_nesting", "Deep nesting; flatten with guard clauses"),
    (
        "poor_naming",
        "Over-abstracted (*Factory), cryptic short name, or magic index",
    ),
    ("noisy_comment", "Comment restates code or leaves TODO/lint noise"),
    ("swallowed_error", "Ignored error or silent fallback value"),
    ("dense_line", "Long line, chained calls, or nested ternary"),
    ("special_casing", "Stacked hardcoded special-case branches"),
    ("duplicate_block", "Duplicated logic block"),
    ("cramped_layout", "Unbroken statement wall or inconsistent indent"),
];

#[derive(Clone)]
struct Tensor {
    shape: Vec<usize>,
    data: Vec<f32>,
}

struct Weights {
    params: HashMap<String, Tensor>,
}

impl Weights {
    fn get(&self, name: &str) -> &Tensor {
        self.params.get(name).unwrap_or_else(|| panic!("missing weight {name}"))
    }
}

fn weights() -> Result<&'static Weights, String> {
    static LOADED: OnceLock<Result<Weights, String>> = OnceLock::new();
    match LOADED.get_or_init(|| load_weights(WEIGHTS_NPZ)) {
        Ok(w) => Ok(w),
        Err(e) => Err(e.clone()),
    }
}

fn u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    let slice: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(slice))
}

fn u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    let slice: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(slice))
}

/// Extracts all entries from a ZIP archive via its Central Directory.
fn unzip_entries(zip: &[u8]) -> Result<HashMap<String, Vec<u8>>, String> {
    let eocd = zip
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x05, 0x06])
        .ok_or("weights.npz: missing ZIP end-of-central-directory")?;
    let cd_size = u32_le(zip, eocd + 12).ok_or("weights.npz: truncated EOCD")? as usize;
    let cd_offset = u32_le(zip, eocd + 16).ok_or("weights.npz: truncated EOCD")? as usize;
    let cd_end = cd_offset.saturating_add(cd_size).min(zip.len());
    let mut out = HashMap::new();
    let mut pos = cd_offset;
    while pos + 46 <= cd_end && &zip[pos..pos + 4] == b"PK\x01\x02" {
        let method = u16_le(zip, pos + 10).ok_or("weights.npz: truncated CD")?;
        let comp_size = u32_le(zip, pos + 20).ok_or("weights.npz: truncated CD")? as usize;
        let name_len = u16_le(zip, pos + 28).ok_or("weights.npz: truncated CD")? as usize;
        let extra_len = u16_le(zip, pos + 30).ok_or("weights.npz: truncated CD")? as usize;
        let comment_len = u16_le(zip, pos + 32).ok_or("weights.npz: truncated CD")? as usize;
        let local_off = u32_le(zip, pos + 42).ok_or("weights.npz: truncated CD")? as usize;
        let name = std::str::from_utf8(zip.get(pos + 46..pos + 46 + name_len).ok_or("weights.npz: bad name")?)
            .map_err(|_| "weights.npz: non-utf8 entry name")?
            .to_string();
        let l_name_len = u16_le(zip, local_off + 26).ok_or("weights.npz: truncated local header")? as usize;
        let l_extra_len = u16_le(zip, local_off + 28).ok_or("weights.npz: truncated local header")? as usize;
        let data_start = local_off + 30 + l_name_len + l_extra_len;
        let compressed = zip
            .get(data_start..data_start + comp_size)
            .ok_or("weights.npz: truncated entry data")?;
        let data = match method {
            0 => compressed.to_vec(),
            8 => {
                let mut decoder = DeflateDecoder::new(compressed);
                let mut buf = Vec::new();
                decoder
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("weights.npz: deflate failed: {e}"))?;
                buf
            }
            other => return Err(format!("weights.npz: unsupported ZIP method {other}")),
        };
        out.insert(name, data);
        pos += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// Strips the `.npy` header and returns the raw payload bytes.
fn npy_payload(npy: &[u8]) -> Result<&[u8], String> {
    if npy.len() < 10 || &npy[..6] != b"\x93NUMPY" {
        return Err("invalid .npy magic".into());
    }
    let major = npy[6];
    let (hstart, hlen) = if major == 1 {
        (10usize, u16_le(npy, 8).ok_or("truncated .npy v1 header")? as usize)
    } else {
        (12usize, u32_le(npy, 8).ok_or("truncated .npy v2 header")? as usize)
    };
    npy.get(hstart + hlen..).ok_or_else(|| "truncated .npy payload".into())
}

fn parse_shape(dims: &str) -> Result<Vec<usize>, String> {
    dims.split(',')
        .map(|d| d.trim().parse::<usize>().map_err(|_| format!("bad dim: {d}")))
        .collect()
}

/// Broadcasts `scale` onto `mat` (matching NumPy trailing-dimension broadcasting).
fn multiply_broadcast(mat: &mut [f32], mat_shape: &[usize], scale: &Tensor) {
    if scale.data.len() == 1 {
        let s = scale.data[0];
        for v in mat.iter_mut() {
            *v *= s;
        }
        return;
    }
    if scale.data.len() == mat.len() {
        for (v, &s) in mat.iter_mut().zip(&scale.data) {
            *v *= s;
        }
        return;
    }
    // Pad scale shape with leading 1s to match mat_shape rank.
    let rank = mat_shape.len();
    let mut s_shape = vec![1usize; rank];
    let offset = rank.saturating_sub(scale.shape.len());
    for (i, &dim) in scale.shape.iter().enumerate() {
        s_shape[offset + i] = dim;
    }
    let mut s_strides = vec![0usize; rank];
    let mut stride = 1usize;
    for i in (0..rank).rev() {
        s_strides[i] = if s_shape[i] == 1 { 0 } else { stride };
        stride *= s_shape[i];
    }
    let mut coords = vec![0usize; rank];
    for val in mat.iter_mut() {
        let mut s_idx = 0usize;
        for d in 0..rank {
            s_idx += coords[d] * s_strides[d];
        }
        *val *= scale.data[s_idx];
        for d in (0..rank).rev() {
            coords[d] += 1;
            if coords[d] < mat_shape[d] {
                break;
            }
            coords[d] = 0;
        }
    }
}

fn load_weights(npz: &[u8]) -> Result<Weights, String> {
    let entries = unzip_entries(npz)?;
    let trit_buf = npy_payload(entries.get("trits.npy").ok_or("missing trits.npy")?)?;
    let vec_bytes = npy_payload(entries.get("vecs.npy").ok_or("missing vecs.npy")?)?;
    let meta_bytes = npy_payload(entries.get("meta.npy").ok_or("missing meta.npy")?)?;

    let vec_buf: Vec<f32> = vec_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| f32::from_le_bytes(c))
        .collect();
    let meta: String = meta_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .filter_map(|&c| {
            let cp = u32::from_le_bytes(c);
            (cp != 0).then(|| char::from_u32(cp)).flatten()
        })
        .collect();

    let (trit_part, vec_part) = meta.split_once('|').ok_or("invalid meta format")?;
    let mut vecs: HashMap<String, Tensor> = HashMap::new();
    let mut vec_offset = 0usize;
    for entry in vec_part.split(" ; ") {
        let mut parts = entry.split(':');
        let name = parts.next().ok_or("bad vec entry")?.to_string();
        let shape = parse_shape(parts.next().ok_or("bad vec dims")?)?;
        let count: usize = parts.next().ok_or("bad vec count")?.parse().map_err(|_| "bad count")?;
        let data = vec_buf
            .get(vec_offset..vec_offset + count)
            .ok_or("vec_buf out of bounds")?
            .to_vec();
        vec_offset += count;
        vecs.insert(name, Tensor { shape, data });
    }

    let mut flat_trits = Vec::with_capacity(trit_buf.len() * 5);
    for &b in trit_buf {
        for &p in &POW3 {
            let digit = (((b / p) % 3 + 1) % 3) as i8 - 1;
            flat_trits.push(f32::from(digit));
        }
    }

    let mut params: HashMap<String, Tensor> = HashMap::new();
    let mut byte_offset = 0usize;
    for entry in trit_part.split(" ; ") {
        let mut parts = entry.split(':');
        let name = parts.next().ok_or("bad trit entry")?.to_string();
        let shape = parse_shape(parts.next().ok_or("bad trit dims")?)?;
        let packed_len: usize = parts
            .next()
            .ok_or("bad packed_len")?
            .parse()
            .map_err(|_| "bad packed_len")?;
        let num_elems: usize = shape.iter().product();
        let start = byte_offset * 5;
        let mut mat = flat_trits
            .get(start..start + num_elems)
            .ok_or("trit_buf out of bounds")?
            .to_vec();
        byte_offset += packed_len;

        let scale_key = if name == "col_sum_proj" {
            "col_sum_s".to_string()
        } else if let Some(stem) = name.strip_suffix("_w") {
            format!("{stem}_s")
        } else {
            format!("{name}_s")
        };
        if let Some(scale) = vecs.get(&scale_key) {
            multiply_broadcast(&mut mat, &shape, scale);
        }
        params.insert(name, Tensor { shape, data: mat });
    }

    for (name, val) in vecs {
        if !name.ends_with("_s") {
            params.insert(name, val);
        }
    }
    Ok(Weights { params })
}

fn expand_tabs_2(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut col = 0usize;
    for ch in code.chars() {
        match ch {
            '\t' => {
                let spaces = 2 - (col % 2);
                for _ in 0..spaces {
                    out.push(' ');
                }
                col += spaces;
            }
            '\n' | '\r' => {
                out.push(ch);
                col = 0;
            }
            _ => {
                out.push(ch);
                col += 1;
            }
        }
    }
    out
}

/// Converts raw source text into `(num_lines, MAX_COLS)` 7-bit ASCII byte grid matching Python `encode_byte_grid`.
fn encode_byte_grid(code: &str) -> (Vec<u8>, usize) {
    let expanded = expand_tabs_2(code);
    let mut lines: Vec<&str> = crate::py::splitlines(&expanded, false);
    if lines.is_empty() {
        lines.push("");
    }
    if lines.len() > MAX_LINES {
        lines.truncate(MAX_LINES);
    }
    let num_lines = lines.len();
    let mut grid = vec![0u8; num_lines * MAX_COLS];
    for (row, text) in lines.iter().enumerate() {
        let base = row * MAX_COLS;
        for (col, ch) in text.chars().take(MAX_COLS).enumerate() {
            let code = (ch as u32).min(127) as u8;
            grid[base + col] = if code == 0 { 32 } else { code };
        }
    }
    (grid, num_lines)
}

#[inline]
fn relu_in_place(data: &mut [f32]) {
    for v in data.iter_mut() {
        if *v < 0.0 {
            *v = 0.0;
        }
    }
}

#[inline]
fn sigmoid(x: f32) -> f32 {
    let c = x.clamp(-30.0, 30.0);
    1.0 / (1.0 + (-c).exp())
}

/// `out = a @ b + bias` where `a` is `(m, k)`, `b` is `(k, n)`, optional `bias` is `(n,)`.
fn matmul_add(a: &[f32], b: &[f32], bias: Option<&[f32]>, m: usize, k: usize, n: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; m * n];
    for i in 0..m {
        let out_row = &mut out[i * n..(i + 1) * n];
        if let Some(bias) = bias {
            out_row.copy_from_slice(bias);
        }
        let a_row = &a[i * k..(i + 1) * k];
        for (p, &av) in a_row.iter().enumerate() {
            if av == 0.0 {
                continue;
            }
            let b_row = &b[p * n..(p + 1) * n];
            for j in 0..n {
                out_row[j] += av * b_row[j];
            }
        }
    }
    out
}

/// 1D convolution along the column axis of `cells` `(L, W, C_in)` with `kernel` `(K, C_in, C_out)` and `bias` `(C_out,)`.
fn conv1d(cells: &[f32], num_lines: usize, num_cols: usize, c_in: usize, kernel: &Tensor, bias: &Tensor) -> Vec<f32> {
    let k_size = kernel.shape[0];
    let c_out = kernel.shape[2];
    let pad = (k_size - 1) / 2;
    // Collect active (offset, in_ch) pairs where the kernel slice has any non-zero weight.
    let mut active: Vec<(usize, usize)> = Vec::new();
    for offset in 0..k_size {
        for ic in 0..c_in {
            let base = (offset * c_in + ic) * c_out;
            if kernel.data[base..base + c_out].iter().any(|&w| w != 0.0) {
                active.push((offset, ic));
            }
        }
    }
    let mut out = vec![0.0f32; num_lines * num_cols * c_out];
    for l in 0..num_lines {
        for w in 0..num_cols {
            let dst = &mut out[(l * num_cols + w) * c_out..(l * num_cols + w + 1) * c_out];
            dst.copy_from_slice(&bias.data);
            for &(offset, ic) in &active {
                let src_w = (w as isize) + (offset as isize) - (pad as isize);
                if src_w < 0 || src_w >= num_cols as isize {
                    continue;
                }
                let val = cells[(l * num_cols + src_w as usize) * c_in + ic];
                if val == 0.0 {
                    continue;
                }
                let k_base = (offset * c_in + ic) * c_out;
                let k_slice = &kernel.data[k_base..k_base + c_out];
                for oc in 0..c_out {
                    dst[oc] += val * k_slice[oc];
                }
            }
        }
    }
    out
}

/// Conv1d + ReLU + sum over columns -> `(L, C_out)`.
fn conv1d_relu_sum(
    cells: &[f32],
    num_lines: usize,
    num_cols: usize,
    c_in: usize,
    kernel: &Tensor,
    bias: &Tensor,
) -> Vec<f32> {
    let c_out = kernel.shape[2];
    let mut conv = conv1d(cells, num_lines, num_cols, c_in, kernel, bias);
    relu_in_place(&mut conv);
    let mut summed = vec![0.0f32; num_lines * c_out];
    for l in 0..num_lines {
        let dst = &mut summed[l * c_out..(l + 1) * c_out];
        for w in 0..num_cols {
            let src = &conv[(l * num_cols + w) * c_out..(l * num_cols + w + 1) * c_out];
            for oc in 0..c_out {
                dst[oc] += src[oc];
            }
        }
    }
    summed
}

/// Concatenates slices row-by-row for `rows` rows.
fn concat_last(rows: usize, parts: &[(&[f32], usize)]) -> Vec<f32> {
    let total_dim: usize = parts.iter().map(|(_, d)| *d).sum();
    let mut out = Vec::with_capacity(rows * total_dim);
    for r in 0..rows {
        for &(slice, dim) in parts {
            out.extend_from_slice(&slice[r * dim..(r + 1) * dim]);
        }
    }
    out
}

fn l2_norm_rows(src: &[f32], rows: usize, dim: usize, eps: f32) -> Vec<f32> {
    let mut out = vec![0.0f32; rows * dim];
    for r in 0..rows {
        let row = &src[r * dim..(r + 1) * dim];
        let norm = (row.iter().map(|&v| v * v).sum::<f32>() + eps).sqrt();
        let dst = &mut out[r * dim..(r + 1) * dim];
        for d in 0..dim {
            dst[d] = row[d] / norm;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn sink_attn(
    state: &[f32],
    num_lines: usize,
    d_model: usize,
    wq: &Tensor,
    wk: &Tensor,
    wv: &Tensor,
    wo: &Tensor,
    b_rel: &Tensor,
    b_sink: &Tensor,
) -> Vec<f32> {
    let num_heads = b_sink.shape[0];
    let q_dim = wq.shape[1];
    let v_dim = wv.shape[1];
    let head_dim = q_dim / num_heads;
    let v_head_dim = v_dim / num_heads;
    let rel_stride = b_rel.shape[1];
    let scale = (head_dim as f32).sqrt();

    let q_proj = matmul_add(state, &wq.data, None, num_lines, d_model, q_dim);
    let k_proj = matmul_add(state, &wk.data, None, num_lines, d_model, q_dim);
    let v_proj = matmul_add(state, &wv.data, None, num_lines, d_model, v_dim);

    let mut merged = vec![0.0f32; num_lines * v_dim];
    let mut logits = vec![0.0f32; num_lines + 1];

    for (h, &sink_logit) in b_sink.data.iter().enumerate().take(num_heads) {
        let h_off = h * head_dim;
        let vh_off = h * v_head_dim;
        let rel_base = h * rel_stride;
        for i in 0..num_lines {
            let qi = &q_proj[i * q_dim + h_off..i * q_dim + h_off + head_dim];
            let mut max_l = sink_logit;
            for j in 0..num_lines {
                let kj = &k_proj[j * q_dim + h_off..j * q_dim + h_off + head_dim];
                let mut dot = 0.0f32;
                for d in 0..head_dim {
                    dot += qi[d] * kj[d];
                }
                let rel_idx = ((j as isize - i as isize).clamp(-MAX_REL_DIST, MAX_REL_DIST) + MAX_REL_DIST) as usize;
                let l = dot / scale + b_rel.data[rel_base + rel_idx];
                logits[j] = l;
                if l > max_l {
                    max_l = l;
                }
            }
            logits[num_lines] = sink_logit;
            let mut sum_exp = 0.0f32;
            for l in logits.iter_mut() {
                *l = (*l - max_l).exp();
                sum_exp += *l;
            }
            let dst = &mut merged[i * v_dim + vh_off..i * v_dim + vh_off + v_head_dim];
            for (j, &exp_w) in logits.iter().enumerate().take(num_lines) {
                let w = exp_w / sum_exp;
                if w == 0.0 {
                    continue;
                }
                let vj = &v_proj[j * v_dim + vh_off..j * v_dim + vh_off + v_head_dim];
                for d in 0..v_head_dim {
                    dst[d] += w * vj[d];
                }
            }
        }
    }
    matmul_add(&merged, &wo.data, None, num_lines, v_dim, wo.shape[1])
}

fn forward_line_probs(w: &Weights, grid: &[u8], num_lines: usize) -> Vec<f32> {
    let tok_emb = w.get("tok_emb");
    let col_emb = w.get("col_emb");
    let emb_dim = tok_emb.shape[1];
    let total_cells = num_lines * MAX_COLS;

    // Block 0: byte/column embeddings + causal quote/comment masks -> cells (L, W, C)
    let mut emb = vec![0.0f32; total_cells * emb_dim];
    for l in 0..num_lines {
        for c in 0..MAX_COLS {
            let tok = grid[l * MAX_COLS + c] as usize;
            let dst = &mut emb[(l * MAX_COLS + c) * emb_dim..(l * MAX_COLS + c + 1) * emb_dim];
            let t_row = &tok_emb.data[tok * emb_dim..(tok + 1) * emb_dim];
            let c_row = &col_emb.data[c * emb_dim..(c + 1) * emb_dim];
            for d in 0..emb_dim {
                dst[d] = t_row[d] + c_row[d];
            }
        }
    }

    let pre_w = w.get("pre_conv3_w");
    let pre_b = w.get("pre_conv3_b");
    let lex_dim = pre_w.shape[2];
    let mut lex = conv1d(&emb, num_lines, MAX_COLS, emb_dim, pre_w, pre_b);
    relu_in_place(&mut lex);

    let lex_cat = concat_last(total_cells, &[(&emb, emb_dim), (&lex, lex_dim)]);
    let pref1_proj = w.get("pref1_proj");
    let pref1_dim = pref1_proj.shape[1];
    let pref1_in = matmul_add(
        &lex_cat,
        &pref1_proj.data,
        None,
        total_cells,
        emb_dim + lex_dim,
        pref1_dim,
    );
    let w_causal1 = w.get("W_causal1");
    let mut prefix1 = vec![0.0f32; total_cells * pref1_dim];
    for l in 0..num_lines {
        let block = matmul_add(
            &w_causal1.data,
            &pref1_in[l * MAX_COLS * pref1_dim..(l + 1) * MAX_COLS * pref1_dim],
            None,
            MAX_COLS,
            MAX_COLS,
            pref1_dim,
        );
        prefix1[l * MAX_COLS * pref1_dim..(l + 1) * MAX_COLS * pref1_dim].copy_from_slice(&block);
    }

    let gate1_in = concat_last(total_cells, &[(&emb, emb_dim), (&lex, lex_dim), (&prefix1, pref1_dim)]);
    let gate1_w = w.get("gate1_w");
    let gate1_b = w.get("gate1_b");
    let gate_dim = gate1_w.shape[1];
    let mut gates = matmul_add(
        &gate1_in,
        &gate1_w.data,
        Some(&gate1_b.data),
        total_cells,
        emb_dim + lex_dim + pref1_dim,
        gate_dim,
    );
    relu_in_place(&mut gates);

    let pref2_proj = w.get("pref2_proj");
    let pref2_dim = pref2_proj.shape[1];
    let pref2_in = matmul_add(&gates, &pref2_proj.data, None, total_cells, gate_dim, pref2_dim);
    let w_causal2 = w.get("W_causal2");
    let mut prefix2 = vec![0.0f32; total_cells * pref2_dim];
    for l in 0..num_lines {
        let block = matmul_add(
            &w_causal2.data,
            &pref2_in[l * MAX_COLS * pref2_dim..(l + 1) * MAX_COLS * pref2_dim],
            None,
            MAX_COLS,
            MAX_COLS,
            pref2_dim,
        );
        prefix2[l * MAX_COLS * pref2_dim..(l + 1) * MAX_COLS * pref2_dim].copy_from_slice(&block);
    }

    let fuse_in = concat_last(
        total_cells,
        &[
            (&emb, emb_dim),
            (&lex, lex_dim),
            (&gates, gate_dim),
            (&prefix2, pref2_dim),
        ],
    );
    let col_fuse_w = w.get("col_fuse_w");
    let col_fuse_b = w.get("col_fuse_b");
    let cell_dim = col_fuse_w.shape[1];
    let mut cells = matmul_add(
        &fuse_in,
        &col_fuse_w.data,
        Some(&col_fuse_b.data),
        total_cells,
        emb_dim + lex_dim + gate_dim + pref2_dim,
        cell_dim,
    );
    relu_in_place(&mut cells);

    // Block 1: k=3/7/13 pattern convolutions + SimHash stem sketches -> state (L, N_Z)
    let conv3 = conv1d_relu_sum(
        &cells,
        num_lines,
        MAX_COLS,
        cell_dim,
        w.get("conv3_w"),
        w.get("conv3_b"),
    );
    let conv7 = conv1d_relu_sum(
        &cells,
        num_lines,
        MAX_COLS,
        cell_dim,
        w.get("conv7_w"),
        w.get("conv7_b"),
    );
    let conv13 = conv1d_relu_sum(
        &cells,
        num_lines,
        MAX_COLS,
        cell_dim,
        w.get("conv13_w"),
        w.get("conv13_b"),
    );

    let mut cell_sums = vec![0.0f32; num_lines * cell_dim];
    for l in 0..num_lines {
        let dst = &mut cell_sums[l * cell_dim..(l + 1) * cell_dim];
        for c in 0..MAX_COLS {
            let src = &cells[(l * MAX_COLS + c) * cell_dim..(l * MAX_COLS + c + 1) * cell_dim];
            for d in 0..cell_dim {
                dst[d] += src[d];
            }
        }
    }
    let col_sum_proj = w.get("col_sum_proj");
    let col_sums = matmul_add(
        &cell_sums,
        &col_sum_proj.data,
        None,
        num_lines,
        cell_dim,
        col_sum_proj.shape[1],
    );

    let raw_sketch = conv1d_relu_sum(
        &cells,
        num_lines,
        MAX_COLS,
        cell_dim,
        w.get("sketch_w"),
        w.get("sketch_b"),
    );
    let sketch_total = w.get("sketch_w").shape[2];
    let sketch: Vec<f32> = raw_sketch.iter().map(|&v| v.clamp(0.0, 1.0)).collect();
    let mut sketch_a = Vec::with_capacity(num_lines * SKETCH_DIM);
    let mut sketch_b = Vec::with_capacity(num_lines * (sketch_total - SKETCH_DIM));
    for l in 0..num_lines {
        let row = &sketch[l * sketch_total..(l + 1) * sketch_total];
        sketch_a.extend_from_slice(&row[..SKETCH_DIM]);
        sketch_b.extend_from_slice(&row[SKETCH_DIM..]);
    }
    let sketch_a_norm = l2_norm_rows(&sketch_a, num_lines, SKETCH_DIM, 2.5);
    let sketch_b_dim = sketch_total - SKETCH_DIM;
    let sketch_b_norm = l2_norm_rows(&sketch_b, num_lines, sketch_b_dim, 2.5);

    let line_feats = concat_last(
        num_lines,
        &[
            (&col_sums, col_sum_proj.shape[1]),
            (&conv3, w.get("conv3_w").shape[2]),
            (&conv7, w.get("conv7_w").shape[2]),
            (&conv13, w.get("conv13_w").shape[2]),
        ],
    );
    let lmlp1_w = w.get("lmlp1_w");
    let lmlp1_b = w.get("lmlp1_b");
    let mut line_hidden = matmul_add(
        &line_feats,
        &lmlp1_w.data,
        Some(&lmlp1_b.data),
        num_lines,
        lmlp1_w.shape[0],
        lmlp1_w.shape[1],
    );
    relu_in_place(&mut line_hidden);
    let lmlp2_w = w.get("lmlp2_w");
    let lmlp2_b = w.get("lmlp2_b");
    let line_stats = matmul_add(
        &line_hidden,
        &lmlp2_w.data,
        Some(&lmlp2_b.data),
        num_lines,
        lmlp2_w.shape[0],
        lmlp2_w.shape[1],
    );

    let mut state = concat_last(
        num_lines,
        &[
            (&line_stats, lmlp2_w.shape[1]),
            (&sketch_a_norm, SKETCH_DIM),
            (&sketch_b_norm, sketch_b_dim),
        ],
    );
    let d_model = lmlp2_w.shape[1] + SKETCH_DIM + sketch_b_dim;

    // Block 2: two cross-line sink-attention + context-MLP passes over [-32..+32] offsets
    for layer in ["1", "2"] {
        let delta = sink_attn(
            &state,
            num_lines,
            d_model,
            w.get(&format!("attn{layer}_wq")),
            w.get(&format!("attn{layer}_wk")),
            w.get(&format!("attn{layer}_wv")),
            w.get(&format!("attn{layer}_wo")),
            w.get(&format!("attn{layer}_brel")),
            w.get(&format!("attn{layer}_bsink")),
        );
        for (s, &d) in state.iter_mut().zip(&delta) {
            *s += d;
        }
        let c1_w = w.get(&format!("cmlp{layer}_w"));
        let c1_b = w.get(&format!("cmlp{layer}_b"));
        let mut ctx_hidden = matmul_add(&state, &c1_w.data, Some(&c1_b.data), num_lines, d_model, c1_w.shape[1]);
        relu_in_place(&mut ctx_hidden);
        let c2_w = w.get(&format!("cmlp{layer}_2_w"));
        let c2_b = w.get(&format!("cmlp{layer}_2_b"));
        let ctx_out = matmul_add(
            &ctx_hidden,
            &c2_w.data,
            Some(&c2_b.data),
            num_lines,
            c2_w.shape[0],
            c2_w.shape[1],
        );
        for (s, &d) in state.iter_mut().zip(&ctx_out) {
            *s += d;
        }
    }

    // Block 3: per-line squiggle probabilities
    let sq_w = w.get("sq_w");
    let sq_b = w.get("sq_b");
    let mut probs = matmul_add(&state, &sq_w.data, Some(&sq_b.data), num_lines, d_model, sq_w.shape[1]);
    for p in probs.iter_mut() {
        *p = sigmoid(*p);
    }
    probs
}

/// Scores a single source file in pure Rust, returning the model's squiggles (threshold 0.55).
pub fn score_code(code: &str) -> Result<Vec<Squiggle>, String> {
    let w = weights()?;
    let (grid, num_lines) = encode_byte_grid(code);
    let probs = forward_line_probs(w, &grid, num_lines);
    let n_kinds = SPEC.len();
    let mut squiggles = Vec::new();
    for idx in 0..num_lines {
        let row = &probs[idx * n_kinds..(idx + 1) * n_kinds];
        let mut best_kind = 0usize;
        let mut best_prob = row[0];
        for (k, &p) in row.iter().enumerate().skip(1) {
            if p > best_prob {
                best_prob = p;
                best_kind = k;
            }
        }
        if best_prob < THRESHOLD {
            continue;
        }
        let (kind, message) = SPEC[best_kind];
        squiggles.push(Squiggle {
            line: idx + 1,
            kind: kind.to_string(),
            message: message.to_string(),
        });
    }
    Ok(squiggles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_rust_hygiene_model_loads_weights_and_flags_swallowed_error() {
        let code = "def load(p):\n    try:\n        return open(p).read()\n    except Exception:\n        pass\n";
        let squiggles = score_code(code).expect("weights load and score cleanly");
        assert!(
            squiggles.iter().any(|s| s.kind == "swallowed_error"),
            "expected swallowed_error in {squiggles:?}"
        );
    }
}
