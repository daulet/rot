#[cfg(rot_host_mode)]
pub fn host_mode_value() -> &'static str {
    "host"
}

#[cfg(rot_target_mode)]
pub fn target_mode_value() -> &'static str {
    "target"
}
