//! K8s Quantity 解析工具（winnow 实现）
//!
//! 解析 K8s 内存/存储 Quantity 字符串（`"512Mi"`、`"1Gi"`、`"1e9"` 等）为字节数。
//! 完整支持 K8s Quantity 规范（BinarySI / DecimalSI / DecimalExponent）。

use winnow::ascii::float;
use winnow::prelude::*;
use winnow::token::rest;

/// 解析 K8s 内存 Quantity（`"512Mi"`、`"1Gi"`、`"1e9"`、`"1024"` 等）为字节数
///
/// 基于 winnow，完整支持 K8s Quantity 规范（apimachinery/pkg/api/resource）：
/// - **BinarySI**：`Ki`/`Mi`/`Gi`/`Ti`/`Pi`/`Ei`（1024 进制）
/// - **DecimalSI**：`k`/`M`/`G`/`T`/`P`/`E`（1000 进制，**`k` 为小写**）+ `m`（毫）
/// - **DecimalExponent**：`e`/`E`[+-]?digits（科学计数法，如 `1e9`，由 `float` 直接解析）
/// - 纯数字（字节）+ 小数（如 `1.5`）
///
/// 非法格式（大写 `K`、负数、未识别后缀、非有限/溢出）返回 `None`。
pub fn parse_memory_quantity(quantity: &str) -> Option<u64> {
    let trimmed = quantity.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut input = trimmed;
    // float 已涵盖 DecimalExponent（"1e9" → 1e9），故后缀查表无需再处理 e/E 分支
    // turbofish 指定 Error=()：解析结果经 `.ok()` 转 Option，不关心错误细节
    let value: f64 = float::<_, f64, ()>.parse_next(&mut input).ok()?;
    let suffix: &str = rest::<_, ()>.parse_next(&mut input).ok()?;
    let multiplier = suffix_to_multiplier(suffix)?;
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    Some(bytes.round() as u64)
}

/// 解析 K8s CPU Quantity（核数，f64）。
///
/// K8s CPU Quantity 仅用 DecimalSI 后缀（无 BinarySI）：
/// - `n` 纳核(1e-9)、`u` 微核(1e-6)、`m` 毫核(1e-3) —— metrics-server 常返回 `123456n`，
///   limits 常为 `500m`
/// - `k`/`M`/`G`/`T`/`P`/`E`（1000 进制）
/// - 无后缀 = 核（如 `"2"`、`"0.5"`）
/// - 科学计数法（`1e3`）由 `float` 直接解析
///
/// 非法（BinarySI 后缀如 `Mi`、负数、未识别后缀、非有限）返回 `None`。
pub fn parse_cpu_quantity(quantity: &str) -> Option<f64> {
    let trimmed = quantity.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut input = trimmed;
    let value: f64 = float::<_, f64, ()>.parse_next(&mut input).ok()?;
    let suffix: &str = rest::<_, ()>.parse_next(&mut input).ok()?;
    let multiplier = cpu_suffix_to_multiplier(suffix)?;
    let cores = value * multiplier;
    if !cores.is_finite() || cores < 0.0 {
        return None;
    }
    Some(cores)
}

/// 校验 K8s 存储大小（Quantity 格式 + 范围限制）。
///
/// 共享校验（app_manager service / rcoder pod_handler 都用），避免逻辑重复。
/// - 格式：合法 K8s Quantity（调 [`parse_memory_quantity`]）
/// - 范围：1Gi ≤ size ≤ 100Ti
pub fn validate_k8s_storage_size(storage_size: &str) -> Result<(), String> {
    let bytes = parse_memory_quantity(storage_size).ok_or_else(|| {
        format!(
            "invalid storage_size '{storage_size}': expected K8s quantity (e.g., 10Gi, 100Mi, 1Ti)"
        )
    })?;
    let gi = bytes as f64 / 1024f64.powi(3);
    if gi < 1.0 {
        return Err("storage_size must be at least 1Gi".to_string());
    }
    if gi > 100.0 * 1024.0 {
        return Err("storage_size cannot exceed 100Ti".to_string());
    }
    Ok(())
}

/// IEC BinarySI 常量（1024 进制，编译期计算，无运行时 powi）
const KIB: f64 = 1024.0;
const MIB: f64 = KIB * KIB;
const GIB: f64 = MIB * KIB;
const TIB: f64 = GIB * KIB;
const PIB: f64 = TIB * KIB;
const EIB: f64 = PIB * KIB;

/// K8s Quantity 后缀 → 乘数；未识别后缀（含大写 `K`）返回 `None`
fn suffix_to_multiplier(suffix: &str) -> Option<f64> {
    Some(match suffix {
        "" => 1.0,
        // BinarySI（1024 进制）
        "Ki" => KIB,
        "Mi" => MIB,
        "Gi" => GIB,
        "Ti" => TIB,
        "Pi" => PIB,
        "Ei" => EIB,
        // DecimalSI（1000 进制）；K8s 用小写 k，大写 K 非法
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        // 毫（DecimalSI）；内存少用但 K8s 支持，结果按需 round
        "m" => 1e-3,
        _ => return None,
    })
}

/// K8s CPU Quantity 后缀 → 乘数（仅 DecimalSI；CPU 无 BinarySI）。
/// metrics-server 用量常为 `n`(纳核),limits 常为 `m`(毫核)或无后缀(核)。
fn cpu_suffix_to_multiplier(suffix: &str) -> Option<f64> {
    Some(match suffix {
        "" => 1.0,
        "n" => 1e-9,
        "u" => 1e-6,
        "m" => 1e-3,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "E" => 1e18,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_si() {
        assert_eq!(parse_memory_quantity("1Ki"), Some(1024));
        assert_eq!(parse_memory_quantity("1Mi"), Some(1024 * 1024));
        assert_eq!(parse_memory_quantity("1Gi"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_quantity("2Gi"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_quantity("1Pi"), Some(1024u64.pow(5)));
    }

    #[test]
    fn parses_decimal_si() {
        assert_eq!(parse_memory_quantity("1k"), Some(1_000));
        assert_eq!(parse_memory_quantity("1M"), Some(1_000_000));
        assert_eq!(parse_memory_quantity("1G"), Some(1_000_000_000));
    }

    #[test]
    fn parses_decimal_exponent_and_plain() {
        assert_eq!(parse_memory_quantity("1e9"), Some(1_000_000_000));
        assert_eq!(parse_memory_quantity("1024"), Some(1024));
        assert_eq!(parse_memory_quantity("1.5"), Some(2)); // 1.5 字节 round → 2
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(parse_memory_quantity("5K"), None); // 大写 K 非法（K8s 用 k）
        assert_eq!(parse_memory_quantity("-5Gi"), None); // 负数
        assert_eq!(parse_memory_quantity("1Xi"), None); // 未识别后缀
        assert_eq!(parse_memory_quantity(""), None);
        assert_eq!(parse_memory_quantity("   "), None);
        assert_eq!(parse_memory_quantity("abc"), None);
    }

    #[test]
    fn parses_cpu_quantity() {
        // 毫核（limits 常见）
        assert_eq!(parse_cpu_quantity("500m"), Some(0.5));
        assert_eq!(parse_cpu_quantity("1000m"), Some(1.0));
        // 纳核（metrics-server 用量常见）
        assert!((parse_cpu_quantity("500000000n").unwrap() - 0.5).abs() < 1e-9);
        // 微核
        assert!((parse_cpu_quantity("500000u").unwrap() - 0.5).abs() < 1e-6);
        // 无后缀 = 核
        assert_eq!(parse_cpu_quantity("2"), Some(2.0));
        assert_eq!(parse_cpu_quantity("0.5"), Some(0.5));
        // 科学计数法
        assert_eq!(parse_cpu_quantity("1e3"), Some(1000.0));
    }

    #[test]
    fn rejects_invalid_cpu_quantity() {
        assert_eq!(parse_cpu_quantity("500Mi"), None); // CPU 无 BinarySI
        assert_eq!(parse_cpu_quantity("-1"), None); // 负数
        assert_eq!(parse_cpu_quantity("1Xi"), None); // 未识别后缀
        assert_eq!(parse_cpu_quantity(""), None);
        assert_eq!(parse_cpu_quantity("abc"), None);
    }

    #[test]
    fn test_validate_k8s_storage_size_valid() {
        assert!(validate_k8s_storage_size("10Gi").is_ok());
        assert!(validate_k8s_storage_size("1Gi").is_ok()); // 边界（最小）
        assert!(validate_k8s_storage_size("100Ti").is_ok()); // 边界（最大）
        assert!(validate_k8s_storage_size("500Gi").is_ok());
    }

    #[test]
    fn test_validate_k8s_storage_size_invalid() {
        assert!(validate_k8s_storage_size("512Mi").is_err()); // < 1Gi
        assert!(validate_k8s_storage_size("0").is_err()); // 太小
        assert!(validate_k8s_storage_size("abc").is_err()); // 非法格式
        assert!(validate_k8s_storage_size("").is_err()); // 空
        assert!(validate_k8s_storage_size("1Pi").is_err()); // > 100Ti（1Pi = 1024Ti）
    }
}
