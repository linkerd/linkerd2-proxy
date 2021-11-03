fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Ensure that at least one TLS implementation feature is enabled.
    static TLS_FEATURES: &[&str] = &["rustls"];
    if !TLS_FEATURES
        .iter()
        .any(|f| std::env::var_os(&*format!("CARGO_FEATURE_{}", f.to_ascii_uppercase())).is_some())
    {
        return Err(format!(
            "at least one of the following TLS implementations must be enabled: '{}'",
            TLS_FEATURES.join("', '"),
        )
        .into());
    }

    Ok(())
}
