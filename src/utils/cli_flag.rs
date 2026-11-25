/// Removes `flag` and its value at `args[index]`, in the `--flag value` or `--flag=value` form.
/// Returns `Ok(None)` when `args[index]` is missing or is another argument.
///
/// # Errors
/// Returns `<flag> needs a value, e.g. <flag> <example>` when the flag has no value.
pub fn take_flag_value(
    args: &mut Vec<String>,
    index: usize,
    flag: &str,
    example: &str,
) -> Result<Option<String>, String> {
    let Some(arg) = args.get(index) else {
        return Ok(None);
    };
    if let Some(value) = arg
        .strip_prefix(flag)
        .and_then(|rest| rest.strip_prefix('='))
    {
        let value = value.to_string();
        args.remove(index);
        return Ok(Some(value));
    }
    if arg != flag {
        return Ok(None);
    }
    let value = args
        .get(index + 1)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value, e.g. {flag} {example}"))?;
    args.drain(index..=index + 1);
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn takes_both_forms_and_leaves_other_arguments() {
        let mut args = strings(&["--x=1", "-p", "--x", "2", "--xy=3"]);

        assert_eq!(
            take_flag_value(&mut args, 0, "--x", "v"),
            Ok(Some("1".to_string()))
        );
        assert_eq!(take_flag_value(&mut args, 0, "--x", "v"), Ok(None));
        assert_eq!(
            take_flag_value(&mut args, 1, "--x", "v"),
            Ok(Some("2".to_string()))
        );
        assert_eq!(take_flag_value(&mut args, 1, "--x", "v"), Ok(None));
        assert_eq!(take_flag_value(&mut args, 5, "--x", "v"), Ok(None));
        assert_eq!(args, ["-p", "--xy=3"]);
    }

    #[test]
    fn a_flag_without_a_value_is_an_error() {
        for args in [&["--x"][..], &["--x", "-p"]] {
            assert_eq!(
                take_flag_value(&mut strings(args), 0, "--x", "v"),
                Err("--x needs a value, e.g. --x v".to_string())
            );
        }
    }
}
