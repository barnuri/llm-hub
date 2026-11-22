//! Memory budget for local model processes.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use super::registry_rows::RegistryRow;

const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// RAM the default budget leaves to the system and other apps.
const DEFAULT_HEADROOM_GB: f64 = 24.0;
/// The default budget never drops below this share of RAM, so a small machine still serves.
const MIN_BUDGET_RAM_SHARE: f64 = 0.5;
/// Estimate of a model whose GGUF header does not give its KV cache shape.
const FALLBACK_ESTIMATE_FACTOR: f64 = 1.1;

const GGUF_MAGIC: &[u8; 4] = b"GGUF";
const GGUF_MIN_VERSION: u32 = 2;
/// The header reader stops at this offset, so a corrupt length cannot make it read the weights.
const GGUF_METADATA_LIMIT_BYTES: u64 = 32 * 1024 * 1024;
/// Integer arrays longer than this (token types, for example) are skipped, not kept.
const GGUF_MAX_KEPT_ARRAY_LEN: u64 = 4096;

const GGUF_TYPE_U8: u32 = 0;
const GGUF_TYPE_I8: u32 = 1;
const GGUF_TYPE_U16: u32 = 2;
const GGUF_TYPE_I16: u32 = 3;
const GGUF_TYPE_U32: u32 = 4;
const GGUF_TYPE_I32: u32 = 5;
const GGUF_TYPE_F32: u32 = 6;
const GGUF_TYPE_BOOL: u32 = 7;
const GGUF_TYPE_STRING: u32 = 8;
const GGUF_TYPE_ARRAY: u32 = 9;
const GGUF_TYPE_U64: u32 = 10;
const GGUF_TYPE_I64: u32 = 11;
const GGUF_TYPE_F64: u32 = 12;

const ARCHITECTURE_KEY: &str = "general.architecture";
const BLOCK_COUNT: &str = "block_count";
const CONTEXT_LENGTH: &str = "context_length";
const EMBEDDING_LENGTH: &str = "embedding_length";
const HEAD_COUNT: &str = "attention.head_count";
const HEAD_COUNT_KV: &str = "attention.head_count_kv";
const KEY_LENGTH: &str = "attention.key_length";
const VALUE_LENGTH: &str = "attention.value_length";
const ARCH_KEY_SUFFIXES: [&str; 7] = [
    BLOCK_COUNT,
    CONTEXT_LENGTH,
    EMBEDDING_LENGTH,
    HEAD_COUNT,
    HEAD_COUNT_KV,
    KEY_LENGTH,
    VALUE_LENGTH,
];

const CACHE_TYPE_K_FLAGS: [&str; 2] = ["--cache-type-k", "-ctk"];
const CACHE_TYPE_V_FLAGS: [&str; 2] = ["--cache-type-v", "-ctv"];
/// llama-server stores the KV cache as f16 unless a flag says otherwise.
const DEFAULT_CACHE_TYPE_BYTES: f64 = 2.0;

/// A model process that holds memory, as the eviction planner sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedModel {
    /// The pool key of the model.
    pub id: String,
    /// The memory estimate of the model in GB.
    pub estimate_gb: f64,
    /// When the model last served a request.
    pub last_used: Instant,
    /// The number of requests the model is serving now.
    pub in_flight: usize,
}

/// Why a new model cannot start now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRoom {
    /// Only models with requests in flight could free enough memory.
    Wait,
    /// The model alone needs more than the whole budget.
    TooBig,
}

/// Total RAM of the machine in bytes, read once per process. `None` on an OS other than macOS
/// and Linux, or when the OS does not report it.
pub fn total_ram_bytes() -> Option<u64> {
    static TOTAL_RAM: OnceLock<Option<u64>> = OnceLock::new();
    *TOTAL_RAM.get_or_init(read_total_ram_bytes)
}

#[cfg(target_os = "macos")]
fn read_total_ram_bytes() -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: the name is a NUL-terminated literal, and `value` is a writable u64 whose size
    // `size` holds, so sysctlbyname writes at most `size` bytes into it.
    let status = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0 && size == std::mem::size_of::<u64>()).then_some(value)
}

#[cfg(target_os = "linux")]
fn read_total_ram_bytes() -> Option<u64> {
    parse_meminfo_total(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_total_ram_bytes() -> Option<u64> {
    None
}

/// Reads `MemTotal: <n> kB` of `/proc/meminfo` text as bytes.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the Linux RAM reader and tests call it")
)]
fn parse_meminfo_total(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let mut parts = line.split_whitespace();
    let kib = parts.next()?.parse::<u64>().ok()?;
    match parts.next() {
        Some("kB") => kib.checked_mul(1024),
        _ => None,
    }
}

/// Memory budget in GB: `configured_gb` (the profile's `MEMORY_GB`) when set, else RAM minus
/// 24 GB but at least half the RAM. `None` when neither is known, which turns the budget off.
pub fn budget_gb(configured_gb: Option<f64>, ram_bytes: Option<u64>) -> Option<f64> {
    if configured_gb.is_some() {
        return configured_gb;
    }
    let ram_gb = to_f64(ram_bytes?) / BYTES_PER_GB;
    Some((ram_gb - DEFAULT_HEADROOM_GB).max(ram_gb * MIN_BUDGET_RAM_SHARE))
}

/// Memory estimate of `row` in GB: its `size_gb` plus the KV cache that its context and its
/// `--cache-type-k/-v` flags need, from the GGUF header of `model_path`. Falls back to
/// `size_gb × 1.1` when the header cannot be read or lacks a key the KV size needs.
pub fn estimate_gb(row: &RegistryRow, model_path: &Path) -> f64 {
    let kv_bytes = read_gguf_metadata(model_path)
        .ok()
        .and_then(|metadata| kv_cache_bytes(&metadata, row));
    match kv_bytes {
        Some(kv_bytes) => row.size_gb + kv_bytes / BYTES_PER_GB,
        None => row.size_gb * FALLBACK_ESTIMATE_FACTOR,
    }
}

/// Picks the models to stop so `needed_gb` fits in `budget_gb`: idle models, least recently
/// used first. Returns an empty list when the model fits already.
///
/// # Errors
///
/// [`NoRoom::TooBig`] when `needed_gb` exceeds the budget alone, and [`NoRoom::Wait`] when
/// stopping every idle model still leaves too little room.
pub fn choose_evictions(
    loaded: &[LoadedModel],
    needed_gb: f64,
    budget_gb: f64,
) -> Result<Vec<&str>, NoRoom> {
    if needed_gb > budget_gb {
        return Err(NoRoom::TooBig);
    }
    let mut free_gb = budget_gb - loaded.iter().map(|model| model.estimate_gb).sum::<f64>();
    let mut idle: Vec<&LoadedModel> = loaded.iter().filter(|model| model.in_flight == 0).collect();
    idle.sort_by_key(|model| model.last_used);
    let mut evicted = Vec::new();
    for model in idle {
        if free_gb >= needed_gb {
            break;
        }
        free_gb += model.estimate_gb;
        evicted.push(model.id.as_str());
    }
    if free_gb >= needed_gb {
        Ok(evicted)
    } else {
        Err(NoRoom::Wait)
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "byte and element counts of RAM and KV caches stay far below 2^53"
)]
fn to_f64(count: u64) -> f64 {
    count as f64
}

/// KV cache bytes of `row`, or `None` when the header lacks a key the size needs.
fn kv_cache_bytes(metadata: &GgufMetadata, row: &RegistryRow) -> Option<f64> {
    let arch = metadata.text(ARCHITECTURE_KEY)?;
    let arch_int = |suffix: &str| metadata.int(&format!("{arch}.{suffix}"));
    let block_count = arch_int(BLOCK_COUNT)?;
    let kv_heads_over_layers = match metadata.get(&format!("{arch}.{HEAD_COUNT_KV}")) {
        Some(MetaValue::Int(heads)) => heads.checked_mul(block_count)?,
        Some(MetaValue::Ints(per_layer)) => per_layer.iter().sum(),
        _ => arch_int(HEAD_COUNT)?.checked_mul(block_count)?,
    };
    let key_length = match arch_int(KEY_LENGTH) {
        Some(length) => length,
        None => arch_int(EMBEDDING_LENGTH)?.checked_div(arch_int(HEAD_COUNT)?)?,
    };
    let value_length = arch_int(VALUE_LENGTH).unwrap_or(key_length);
    let ctx = match row.ctx {
        0 => arch_int(CONTEXT_LENGTH)?,
        ctx => ctx,
    };
    let k_bytes = cache_type_bytes(&row.extra_flags, &CACHE_TYPE_K_FLAGS);
    let v_bytes = cache_type_bytes(&row.extra_flags, &CACHE_TYPE_V_FLAGS);
    let bytes_per_head_token = to_f64(key_length) * k_bytes + to_f64(value_length) * v_bytes;
    Some(to_f64(kv_heads_over_layers) * to_f64(ctx) * bytes_per_head_token)
}

/// Bytes per KV cache element of the last of `flags` in `extra_flags`; f16 when none is set or
/// the type is unknown, which overestimates a smaller type.
fn cache_type_bytes(extra_flags: &str, flags: &[&str]) -> f64 {
    let mut tokens = extra_flags.split_whitespace();
    let mut cache_type = None;
    while let Some(token) = tokens.next() {
        for flag in flags {
            if token == *flag {
                cache_type = tokens.next().or(cache_type);
            } else if let Some(value) = token
                .strip_prefix(flag)
                .and_then(|rest| rest.strip_prefix('='))
            {
                cache_type = Some(value);
            }
        }
    }
    match cache_type.map(str::to_ascii_lowercase).as_deref() {
        Some("f32") => 4.0,
        Some("q8_0") => 1.0,
        Some("q4_0") => 0.5,
        _ => DEFAULT_CACHE_TYPE_BYTES,
    }
}

/// A metadata value the estimate may need. Other values are skipped while reading.
#[derive(Debug, Clone, PartialEq)]
enum MetaValue {
    Int(u64),
    Ints(Vec<u64>),
    Text(String),
    Skipped,
}

#[derive(Debug, Default)]
struct GgufMetadata(HashMap<String, MetaValue>);

impl GgufMetadata {
    fn get(&self, key: &str) -> Option<&MetaValue> {
        self.0.get(key)
    }

    fn int(&self, key: &str) -> Option<u64> {
        match self.get(key)? {
            MetaValue::Int(value) => Some(*value),
            _ => None,
        }
    }

    fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            MetaValue::Text(value) => Some(value),
            _ => None,
        }
    }

    /// True once the architecture and every key the KV size may read are known.
    fn has_all_arch_keys(&self) -> bool {
        self.text(ARCHITECTURE_KEY).is_some_and(|arch| {
            ARCH_KEY_SUFFIXES
                .iter()
                .all(|suffix| self.0.contains_key(&format!("{arch}.{suffix}")))
        })
    }
}

/// Reads the key-value section of the GGUF header at `path`, at most 32 MB from the start.
fn read_gguf_metadata(path: &Path) -> io::Result<GgufMetadata> {
    let file = BufReader::new(File::open(path)?).take(GGUF_METADATA_LIMIT_BYTES);
    GgufReader(file).metadata()
}

struct GgufReader<R>(R);

impl<R: Read> GgufReader<R> {
    fn metadata(&mut self) -> io::Result<GgufMetadata> {
        let mut magic = [0u8; 4];
        self.0.read_exact(&mut magic)?;
        if &magic != GGUF_MAGIC {
            return Err(invalid_data("the file is not GGUF"));
        }
        if self.u32()? < GGUF_MIN_VERSION {
            return Err(invalid_data("GGUF version 1 is not supported"));
        }
        let _tensor_count = self.u64()?;
        let kv_count = self.u64()?;
        let mut metadata = GgufMetadata::default();
        for _ in 0..kv_count {
            let key = self.string()?;
            let value_type = self.u32()?;
            let value = self.value(value_type)?;
            if value != MetaValue::Skipped {
                metadata.0.insert(key, value);
            }
            if metadata.has_all_arch_keys() {
                break;
            }
        }
        Ok(metadata)
    }

    fn value(&mut self, value_type: u32) -> io::Result<MetaValue> {
        if is_int_type(value_type) {
            return Ok(self
                .int(value_type)?
                .map_or(MetaValue::Skipped, MetaValue::Int));
        }
        match value_type {
            GGUF_TYPE_STRING => Ok(MetaValue::Text(self.string()?)),
            GGUF_TYPE_ARRAY => self.array(),
            other => {
                self.skip(fixed_size(other)?)?;
                Ok(MetaValue::Skipped)
            }
        }
    }

    /// Reads an integer of `value_type`, an integer type; `None` for a negative value.
    fn int(&mut self, value_type: u32) -> io::Result<Option<u64>> {
        let value = match value_type {
            GGUF_TYPE_U8 => i128::from(u8::from_le_bytes(self.array_of()?)),
            GGUF_TYPE_I8 => i128::from(i8::from_le_bytes(self.array_of()?)),
            GGUF_TYPE_U16 => i128::from(u16::from_le_bytes(self.array_of()?)),
            GGUF_TYPE_I16 => i128::from(i16::from_le_bytes(self.array_of()?)),
            GGUF_TYPE_U32 => i128::from(self.u32()?),
            GGUF_TYPE_I32 => i128::from(i32::from_le_bytes(self.array_of()?)),
            GGUF_TYPE_I64 => i128::from(i64::from_le_bytes(self.array_of()?)),
            _ => i128::from(self.u64()?),
        };
        Ok(u64::try_from(value).ok())
    }

    fn array(&mut self) -> io::Result<MetaValue> {
        let element_type = self.u32()?;
        let count = self.u64()?;
        if is_int_type(element_type) && count <= GGUF_MAX_KEPT_ARRAY_LEN {
            let mut ints = Vec::new();
            let mut all_non_negative = true;
            for _ in 0..count {
                match self.int(element_type)? {
                    Some(value) => ints.push(value),
                    None => all_non_negative = false,
                }
            }
            return Ok(if all_non_negative {
                MetaValue::Ints(ints)
            } else {
                MetaValue::Skipped
            });
        }
        match element_type {
            GGUF_TYPE_STRING => {
                for _ in 0..count {
                    let length = self.u64()?;
                    self.skip(length)?;
                }
            }
            GGUF_TYPE_ARRAY => return Err(invalid_data("nested GGUF arrays are not supported")),
            other => {
                let bytes = count
                    .checked_mul(fixed_size(other)?)
                    .ok_or_else(|| invalid_data("GGUF array length overflows"))?;
                self.skip(bytes)?;
            }
        }
        Ok(MetaValue::Skipped)
    }

    fn array_of<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut bytes = [0u8; N];
        self.0.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.array_of()?))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.array_of()?))
    }

    fn string(&mut self) -> io::Result<String> {
        let length = self.u64()?;
        let mut bytes = Vec::new();
        let read = (&mut self.0).take(length).read_to_end(&mut bytes)?;
        if u64::try_from(read).ok() != Some(length) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        String::from_utf8(bytes).map_err(|_| invalid_data("a GGUF string is not UTF-8"))
    }

    fn skip(&mut self, length: u64) -> io::Result<()> {
        let skipped = io::copy(&mut (&mut self.0).take(length), &mut io::sink())?;
        if skipped == length {
            Ok(())
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        }
    }
}

fn is_int_type(value_type: u32) -> bool {
    matches!(
        value_type,
        GGUF_TYPE_U8
            | GGUF_TYPE_I8
            | GGUF_TYPE_U16
            | GGUF_TYPE_I16
            | GGUF_TYPE_U32
            | GGUF_TYPE_I32
            | GGUF_TYPE_U64
            | GGUF_TYPE_I64
    )
}

/// Size in bytes of a fixed-size GGUF value type.
fn fixed_size(value_type: u32) -> io::Result<u64> {
    match value_type {
        GGUF_TYPE_U8 | GGUF_TYPE_I8 | GGUF_TYPE_BOOL => Ok(1),
        GGUF_TYPE_U16 | GGUF_TYPE_I16 => Ok(2),
        GGUF_TYPE_U32 | GGUF_TYPE_I32 | GGUF_TYPE_F32 => Ok(4),
        GGUF_TYPE_U64 | GGUF_TYPE_I64 | GGUF_TYPE_F64 => Ok(8),
        _ => Err(invalid_data("unknown GGUF value type")),
    }
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::services::local_serving::registry_rows::Engine;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[derive(Default)]
    struct GgufBuilder {
        kvs: Vec<u8>,
        count: u64,
    }

    impl GgufBuilder {
        fn key(mut self, key: &str, value_type: u32) -> Self {
            self.kvs.extend(gguf_string(key));
            self.kvs.extend(value_type.to_le_bytes());
            self.count += 1;
            self
        }

        fn u32(self, key: &str, value: u32) -> Self {
            let mut this = self.key(key, GGUF_TYPE_U32);
            this.kvs.extend(value.to_le_bytes());
            this
        }

        fn u64(self, key: &str, value: u64) -> Self {
            let mut this = self.key(key, GGUF_TYPE_U64);
            this.kvs.extend(value.to_le_bytes());
            this
        }

        fn i32(self, key: &str, value: i32) -> Self {
            let mut this = self.key(key, GGUF_TYPE_I32);
            this.kvs.extend(value.to_le_bytes());
            this
        }

        fn raw(self, key: &str, value_type: u32, bytes: &[u8]) -> Self {
            let mut this = self.key(key, value_type);
            this.kvs.extend(bytes);
            this
        }

        fn text(self, key: &str, value: &str) -> Self {
            let mut this = self.key(key, GGUF_TYPE_STRING);
            this.kvs.extend(gguf_string(value));
            this
        }

        fn array(self, key: &str, element_type: u32, count: u64, elements: &[u8]) -> Self {
            let mut this = self.key(key, GGUF_TYPE_ARRAY);
            this.kvs.extend(element_type.to_le_bytes());
            this.kvs.extend(count.to_le_bytes());
            this.kvs.extend(elements);
            this
        }

        fn build_version(&self, version: u32, extra_kv_count: u64) -> Vec<u8> {
            let mut bytes = GGUF_MAGIC.to_vec();
            bytes.extend(version.to_le_bytes());
            bytes.extend(0u64.to_le_bytes());
            bytes.extend((self.count + extra_kv_count).to_le_bytes());
            bytes.extend(&self.kvs);
            bytes
        }

        fn build(&self) -> Vec<u8> {
            self.build_version(3, 0)
        }
    }

    fn gguf_string(value: &str) -> Vec<u8> {
        let mut bytes = (value.len() as u64).to_le_bytes().to_vec();
        bytes.extend(value.as_bytes());
        bytes
    }

    fn le_u32s(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// A llama header with unrelated keys of every skipped kind before the attention keys.
    fn llama_header() -> GgufBuilder {
        let tokens: Vec<u8> = ["<s>", "</s>", "hello"]
            .iter()
            .flat_map(|t| gguf_string(t))
            .collect();
        GgufBuilder::default()
            .text("general.architecture", "llama")
            .raw(
                "general.quantization_version_f",
                GGUF_TYPE_F32,
                &1.5f32.to_le_bytes(),
            )
            .raw("general.flag", GGUF_TYPE_BOOL, &[1])
            .raw("general.score", GGUF_TYPE_F64, &2.0f64.to_le_bytes())
            .raw("general.small", GGUF_TYPE_U8, &[7])
            .raw("general.signed_small", GGUF_TYPE_I8, &[0xFF])
            .raw("general.u16", GGUF_TYPE_U16, &3u16.to_le_bytes())
            .raw("general.i16", GGUF_TYPE_I16, &(-3i16).to_le_bytes())
            .raw("general.i64", GGUF_TYPE_I64, &9i64.to_le_bytes())
            .array("tokenizer.ggml.tokens", GGUF_TYPE_STRING, 3, &tokens)
            .array("tokenizer.ggml.scores", GGUF_TYPE_F32, 2, &[0; 8])
            .array(
                "tokenizer.ggml.negative",
                GGUF_TYPE_I32,
                2,
                &[1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF],
            )
            .array(
                "tokenizer.ggml.long",
                GGUF_TYPE_U8,
                GGUF_MAX_KEPT_ARRAY_LEN + 1,
                &[0; 4097],
            )
            .u32("llama.block_count", 4)
            .u32("llama.attention.head_count", 8)
            .u32("llama.attention.head_count_kv", 2)
            .u64("llama.embedding_length", 512)
            .u32("llama.context_length", 8192)
    }

    fn write_model(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn row(size_gb: f64, ctx: u64, extra_flags: &str) -> RegistryRow {
        RegistryRow {
            id: "m".to_owned(),
            repo: "org/repo".to_owned(),
            file: "model.gguf".to_owned(),
            size_gb,
            ctx,
            quant: String::new(),
            extra_flags: extra_flags.to_owned(),
            bin_override: None,
            engine: Engine::Llama,
        }
    }

    /// 4 layers × 2 KV heads × head size 64 for K and V.
    fn llama_kv_gb(ctx: u64, bytes_per_element: f64) -> f64 {
        to_f64(4 * 2 * ctx * (64 + 64)) * bytes_per_element / BYTES_PER_GB
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    #[test]
    fn configured_budget_wins_over_ram() {
        assert_eq!(budget_gb(Some(40.0), Some(128 * GIB)), Some(40.0));
        assert_eq!(budget_gb(Some(40.0), None), Some(40.0));
    }

    #[test]
    fn default_budget_is_ram_minus_24_gb_floored_at_half_the_ram() {
        assert_eq!(budget_gb(None, Some(128 * GIB)), Some(104.0));
        assert_eq!(budget_gb(None, Some(32 * GIB)), Some(16.0));
        assert_eq!(budget_gb(None, None), None);
    }

    #[test]
    fn meminfo_total_is_read_in_bytes() {
        let meminfo = "MemFree:  100 kB\nMemTotal:       16384 kB\n";
        assert_eq!(parse_meminfo_total(meminfo), Some(16384 * 1024));
        assert_eq!(parse_meminfo_total("MemFree: 1 kB\n"), None);
        assert_eq!(parse_meminfo_total("MemTotal: 1 MB\n"), None);
        assert_eq!(parse_meminfo_total("MemTotal: lots kB\n"), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn total_ram_is_known_and_stable() {
        let ram = total_ram_bytes().unwrap();
        assert!(ram > GIB);
        assert_eq!(total_ram_bytes(), Some(ram));
    }

    #[test]
    fn f16_kv_cache_is_added_by_default() {
        let (_dir, path) = write_model(&llama_header().build());
        let estimate = estimate_gb(&row(5.0, 4096, "--jinja"), &path);
        assert_close(estimate, 5.0 + llama_kv_gb(4096, 2.0));
    }

    #[test]
    fn q8_0_kv_cache_takes_one_byte_per_element() {
        let (_dir, path) = write_model(&llama_header().build());
        let flags = "--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on";
        let estimate = estimate_gb(&row(5.0, 4096, flags), &path);
        assert_close(estimate, 5.0 + llama_kv_gb(4096, 1.0));
    }

    #[test]
    fn zero_ctx_uses_the_header_context_length() {
        let (_dir, path) = write_model(&llama_header().build());
        let estimate = estimate_gb(&row(5.0, 0, ""), &path);
        assert_close(estimate, 5.0 + llama_kv_gb(8192, 2.0));
    }

    #[test]
    fn explicit_lengths_and_per_layer_kv_heads_are_used() {
        let header = GgufBuilder::default()
            .text("general.architecture", "hybrid")
            .u32("hybrid.block_count", 3)
            .array(
                "hybrid.attention.head_count_kv",
                GGUF_TYPE_U32,
                3,
                &le_u32s(&[4, 0, 4]),
            )
            .u32("hybrid.attention.key_length", 128)
            .u32("hybrid.attention.value_length", 64)
            .build();
        let (_dir, path) = write_model(&header);
        let estimate = estimate_gb(&row(1.0, 100, "-ctk=q4_0 -ctv f32"), &path);
        let kv_bytes = 8.0 * 100.0 * (128.0 * 0.5 + 64.0 * 4.0);
        assert_close(estimate, 1.0 + kv_bytes / BYTES_PER_GB);
    }

    #[test]
    fn missing_head_count_kv_falls_back_to_head_count() {
        let header = GgufBuilder::default()
            .text("general.architecture", "mha")
            .u32("mha.block_count", 2)
            .u32("mha.attention.head_count", 4)
            .u32("mha.embedding_length", 256)
            .build();
        let (_dir, path) = write_model(&header);
        let estimate = estimate_gb(&row(1.0, 10, ""), &path);
        assert_close(
            estimate,
            1.0 + 8.0 * 10.0 * (64.0 * 2.0 * 2.0) / BYTES_PER_GB,
        );
    }

    #[test]
    fn reading_stops_once_every_key_is_known() {
        let mut bytes = llama_header()
            .u32("llama.attention.key_length", 64)
            .u32("llama.attention.value_length", 64)
            .build_version(3, 5);
        bytes.extend([0xAB; 3]);
        let (_dir, path) = write_model(&bytes);
        let estimate = estimate_gb(&row(5.0, 4096, ""), &path);
        assert_close(estimate, 5.0 + llama_kv_gb(4096, 2.0));
    }

    #[test]
    fn unreadable_or_incomplete_headers_fall_back_to_size_times_1_1() {
        let missing_block_count = GgufBuilder::default()
            .text("general.architecture", "llama")
            .u32("llama.attention.head_count", 8)
            .u32("llama.embedding_length", 512)
            .build();
        let negative_block_count = GgufBuilder::default()
            .text("general.architecture", "llama")
            .i32("llama.block_count", -1)
            .u32("llama.attention.head_count", 8)
            .u32("llama.embedding_length", 512)
            .build();
        let no_architecture = GgufBuilder::default().u32("llama.block_count", 4).build();
        let nested_array = GgufBuilder::default()
            .array("weird", GGUF_TYPE_ARRAY, GGUF_MAX_KEPT_ARRAY_LEN + 1, &[])
            .build();
        let unknown_type = GgufBuilder::default().raw("weird", 99, &[]).build();
        let bad_utf8 = GgufBuilder::default()
            .key("general.architecture", GGUF_TYPE_STRING)
            .kvs
            .into_iter()
            .chain(2u64.to_le_bytes())
            .chain([0xC3, 0x28])
            .collect::<Vec<u8>>();
        let mut bad_utf8_file = GGUF_MAGIC.to_vec();
        bad_utf8_file.extend(3u32.to_le_bytes());
        bad_utf8_file.extend(0u64.to_le_bytes());
        bad_utf8_file.extend(1u64.to_le_bytes());
        bad_utf8_file.extend(bad_utf8);
        let mut truncated = llama_header().build();
        truncated.truncate(truncated.len() - 2);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("missing block_count", missing_block_count),
            ("negative block_count", negative_block_count),
            ("no architecture", no_architecture),
            ("nested array", nested_array),
            ("unknown type", unknown_type),
            ("bad utf8", bad_utf8_file),
            ("truncated", truncated),
            ("version 1", llama_header().build_version(1, 0)),
            ("not gguf", b"GGML and more bytes".to_vec()),
        ];
        for (name, bytes) in cases {
            let (_dir, path) = write_model(&bytes);
            let estimate = estimate_gb(&row(10.0, 4096, ""), &path);
            assert!((estimate - 11.0).abs() < 1e-12, "{name}: {estimate}");
        }
        let missing = Path::new("/nonexistent/model.gguf");
        assert_close(estimate_gb(&row(10.0, 4096, ""), missing), 11.0);
    }

    #[test]
    fn truncated_string_array_falls_back() {
        let header = GgufBuilder::default()
            .array(
                "tokenizer.ggml.tokens",
                GGUF_TYPE_STRING,
                2,
                &gguf_string("a"),
            )
            .build();
        let (_dir, path) = write_model(&header);
        assert_close(estimate_gb(&row(10.0, 1, ""), &path), 11.0);
    }

    #[test]
    fn cache_type_reads_every_flag_form_and_the_last_wins() {
        assert_close(cache_type_bytes("", &CACHE_TYPE_K_FLAGS), 2.0);
        assert_close(cache_type_bytes("-ctk Q8_0", &CACHE_TYPE_K_FLAGS), 1.0);
        assert_close(
            cache_type_bytes("--cache-type-k=q4_0", &CACHE_TYPE_K_FLAGS),
            0.5,
        );
        assert_close(
            cache_type_bytes("--cache-type-k f32", &CACHE_TYPE_K_FLAGS),
            4.0,
        );
        assert_close(
            cache_type_bytes("--cache-type-k iq4_nl", &CACHE_TYPE_K_FLAGS),
            2.0,
        );
        assert_close(
            cache_type_bytes("-ctk q8_0 -ctk q4_0", &CACHE_TYPE_K_FLAGS),
            0.5,
        );
        assert_close(cache_type_bytes("-ctk q8_0 -ctk", &CACHE_TYPE_K_FLAGS), 1.0);
        assert_close(cache_type_bytes("-ctv q8_0", &CACHE_TYPE_K_FLAGS), 2.0);
    }

    /// A model last used `used_at_secs` after a shared start, so a larger value is more recent.
    fn loaded(id: &str, estimate_gb: f64, used_at_secs: u64, in_flight: usize) -> LoadedModel {
        static START: OnceLock<Instant> = OnceLock::new();
        LoadedModel {
            id: id.to_owned(),
            estimate_gb,
            last_used: *START.get_or_init(Instant::now) + Duration::from_secs(used_at_secs),
            in_flight,
        }
    }

    #[test]
    fn a_model_that_fits_evicts_nothing() {
        let models = [loaded("a", 10.0, 5, 0)];
        assert_eq!(choose_evictions(&models, 20.0, 40.0), Ok(vec![]));
        assert_eq!(choose_evictions(&[], 40.0, 40.0), Ok(vec![]));
    }

    #[test]
    fn idle_models_are_evicted_least_recently_used_first() {
        let models = [
            loaded("recent", 10.0, 990, 0),
            loaded("busy", 10.0, 500, 2),
            loaded("oldest", 10.0, 700, 0),
            loaded("older", 10.0, 800, 0),
        ];
        assert_eq!(choose_evictions(&models, 10.0, 45.0), Ok(vec!["oldest"]));
        assert_eq!(
            choose_evictions(&models, 25.0, 45.0),
            Ok(vec!["oldest", "older"])
        );
    }

    #[test]
    fn only_busy_models_left_means_wait() {
        let models = [loaded("idle", 10.0, 10, 0), loaded("busy", 25.0, 500, 1)];
        assert_eq!(choose_evictions(&models, 20.0, 40.0), Err(NoRoom::Wait));
    }

    #[test]
    fn a_model_above_the_budget_is_too_big() {
        let models = [loaded("idle", 10.0, 10, 0)];
        assert_eq!(choose_evictions(&models, 40.5, 40.0), Err(NoRoom::TooBig));
    }
}
