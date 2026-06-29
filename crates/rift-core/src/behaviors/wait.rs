//! Wait behavior - add latency before response.

use serde::{Deserialize, Serialize};

/// Wait behavior - add latency before response
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum WaitBehavior {
    /// Fixed delay in milliseconds
    Fixed(u64),
    /// Random delay within range
    Range {
        #[serde(rename = "min")]
        min_ms: u64,
        #[serde(rename = "max")]
        max_ms: u64,
    },
    /// JavaScript function that returns delay
    Function(String),
}

impl WaitBehavior {
    /// Get the wait duration in milliseconds
    pub fn get_duration_ms(&self) -> u64 {
        match self {
            WaitBehavior::Fixed(ms) => *ms,
            WaitBehavior::Range { min_ms, max_ms } => {
                use rand::Rng;
                rand::thread_rng().gen_range(*min_ms..=*max_ms)
            }
            WaitBehavior::Function(js_func) => {
                // Parse JavaScript function and execute
                // Format: "function() { return Math.floor(Math.random() * 100) + 50; }"
                Self::execute_js_wait_function(js_func).unwrap_or(100)
            }
        }
    }

    /// Execute a JavaScript wait function
    fn execute_js_wait_function(js_func: &str) -> Option<u64> {
        // Extract the function body
        let trimmed = js_func.trim();
        if !trimmed.starts_with("function") {
            return None;
        }

        if let Some(body) = extract_function_body(trimmed) {
            // Handle Solo pattern:
            // var min = Math.ceil(N); var max = Math.floor(M); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait;
            if body.contains("var min") && body.contains("var max") {
                return Self::parse_solo_wait_pattern(&body);
            }

            // Look for patterns like "Math.floor(Math.random() * 100) + 50"
            // or "return Math.floor(Math.random() * 100) + 50;"
            let body = body
                .replace("return ", "")
                .trim_end_matches(';')
                .to_string();

            // Parse: Math.floor(Math.random() * N) + M
            if body.contains("Math.random()") {
                use rand::Rng;
                // Extract multiplier and offset using regex
                let re = regex::Regex::new(
                    r"Math\.floor\s*\(\s*Math\.random\s*\(\s*\)\s*\*\s*(\d+)\s*\)\s*\+\s*(\d+)",
                )
                .ok()?;

                if let Some(caps) = re.captures(&body) {
                    let range = caps.get(1)?.as_str().parse::<u64>().ok()?;
                    let offset = caps.get(2)?.as_str().parse::<u64>().ok()?;
                    return Some(rand::thread_rng().gen_range(offset..=offset + range));
                }

                // Simpler pattern: Math.random() * N
                let re = regex::Regex::new(r"Math\.random\s*\(\s*\)\s*\*\s*(\d+)").ok()?;
                if let Some(caps) = re.captures(&body) {
                    let range = caps.get(1)?.as_str().parse::<u64>().ok()?;
                    return Some(rand::thread_rng().gen_range(0..=range));
                }
            }

            // Try to parse as simple number
            body.trim().parse::<u64>().ok()
        } else {
            None
        }
    }

    /// Parse Solo wait pattern:
    /// var min = Math.ceil(N); var max = Math.floor(M); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait;
    fn parse_solo_wait_pattern(body: &str) -> Option<u64> {
        use rand::Rng;

        // Extract min value: var min = Math.ceil(N)
        let min_re = regex::Regex::new(r"var\s+min\s*=\s*Math\.ceil\s*\(\s*(\d+)\s*\)").ok()?;
        let min_val = min_re
            .captures(body)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);

        // Extract max value: var max = Math.floor(N)
        let max_re = regex::Regex::new(r"var\s+max\s*=\s*Math\.floor\s*\(\s*(\d+)\s*\)").ok()?;
        let max_val = max_re
            .captures(body)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);

        // Generate random value in range [min, max]
        if max_val >= min_val {
            Some(rand::thread_rng().gen_range(min_val..=max_val))
        } else {
            Some(min_val)
        }
    }
}

/// Extract function body from JavaScript function string
fn extract_function_body(js_func: &str) -> Option<String> {
    let start = js_func.find('{')?;
    let end = js_func.rfind('}')?;
    if start < end {
        Some(js_func[start + 1..end].trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wait_behavior_fixed() {
        let wait = WaitBehavior::Fixed(100);
        assert_eq!(wait.get_duration_ms(), 100);
    }

    #[test]
    fn test_wait_behavior_range() {
        let wait = WaitBehavior::Range {
            min_ms: 100,
            max_ms: 200,
        };
        for _ in 0..10 {
            let duration = wait.get_duration_ms();
            assert!((100..=200).contains(&duration));
        }
    }

    #[test]
    fn test_wait_behavior_serde() {
        let yaml = "100";
        let wait: WaitBehavior = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(wait, WaitBehavior::Fixed(100)));

        let yaml = "min: 100\nmax: 200";
        let wait: WaitBehavior = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            wait,
            WaitBehavior::Range {
                min_ms: 100,
                max_ms: 200
            }
        ));
    }

    #[test]
    fn test_wait_behavior_solo_js_pattern() {
        // Solo pattern with min=0, max=0 -> returns 0
        let js = " function() { var min = Math.ceil(0); var max = Math.floor(0); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait; } ";
        let wait = WaitBehavior::Function(js.to_string());
        assert_eq!(wait.get_duration_ms(), 0);

        // Solo pattern with min=50, max=100 -> returns value in range
        let js = "function() { var min = Math.ceil(50); var max = Math.floor(100); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait; }";
        let wait = WaitBehavior::Function(js.to_string());
        for _ in 0..10 {
            let duration = wait.get_duration_ms();
            assert!(
                (50..=100).contains(&duration),
                "Duration {} not in range 50-100",
                duration
            );
        }
    }

    #[test]
    fn test_wait_behavior_js_function() {
        // Simple random pattern
        let js = "function() { return Math.floor(Math.random() * 100) + 50; }";
        let wait = WaitBehavior::Function(js.to_string());
        for _ in 0..10 {
            let duration = wait.get_duration_ms();
            assert!(
                (50..=150).contains(&duration),
                "Duration {} not in range 50-150",
                duration
            );
        }
    }
}
