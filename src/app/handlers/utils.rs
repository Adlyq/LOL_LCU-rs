use serde_json::Value;

pub fn event_data(event: &Value) -> Option<&Value> {
    let data = event.get("data")?;
    if data.is_object() || data.is_string() { Some(data) } else { None }
}

pub fn is_overlay_forced() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("LOL_LCU_SHOW_OVERLAY").is_ok()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}
