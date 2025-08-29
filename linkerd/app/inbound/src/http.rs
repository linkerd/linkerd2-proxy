mod router;
mod server;
mod set_identity_header;
#[cfg(test)]
mod tests;

#[cfg(fuzzing)]
pub mod fuzz;

fn trace_labels() -> std::collections::HashMap<String, String> {
    let mut l = std::collections::HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}
