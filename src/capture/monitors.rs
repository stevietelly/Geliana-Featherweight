use windows_capture::monitor::Monitor;

/// Information about a display monitor for UI selection.
///
/// Each monitor is identified by a 1-based index that matches the UI label.
pub struct MonitorInfo {
    /// 1-based index of the monitor (matches the UI label).
    pub index: usize,
    /// Human-readable label including dimensions.
    pub label: String,
}

/// Enumerates available monitors and returns them as `MonitorInfo` objects.
///
/// If no monitors are found, a default "Primary Display" monitor is added.
pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    let mut result = Monitor::enumerate()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(position, monitor)| {
            let index = monitor.index().unwrap_or(position + 1);
            // strip of spaces from the monitor name
            let mut name = monitor.name()
                .unwrap_or_else(|_| format!("{} {index}", "Display"))
                .trim()
                .to_string();

            let dimensions = match (monitor.width(), monitor.height()) {
                (Ok(width), Ok(height)) => format!(" — {width}×{height}"),
                _ => String::new(),
            };
            // if index is 0 and no name we assume it's the primary display
            if index == 0 && name.is_empty() {
                name = "Primary Display".to_string();
                println!("Monitor {index}: {name}{dimensions}");
            }
            MonitorInfo {
                index,
                label: format!("{name}{dimensions}"),
            }
        })
        .collect::<Vec<_>>();

    if result.is_empty() {
        result.push(MonitorInfo {
            index: 1,
            label: "Primary Display".to_string(),
        });
    }

    for (index, monitor) in result.iter_mut().enumerate() {
        println!("Monitor {index}: {}", monitor.label);
    }

    result
}
