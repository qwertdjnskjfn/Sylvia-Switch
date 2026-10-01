# -*- coding: utf-8 -*-
"""一次性补丁：Sylvia Switch 全网关模型上下文拉到最长宣称值。
新增 config::max_context_window(model) 家族映射 helper，并接管：
glm_proxy.meta_summary / glmplan models / glm_web models / workbuddy_upstream model_entry。
"""
import io

def patch(path, edits):
    t = io.open(path, encoding='utf-8').read()
    for i, (old, new) in enumerate(edits):
        assert old in t, f'{path} anchor#{i} not found: {old[:60]!r}'
        t = t.replace(old, new, 1)
    io.open(p := path, 'w', encoding='utf-8').write(t)
    print(f'{path}: {len(edits)} edits ok')

# ── 1) config.rs：全局 helper ─────────────────────────────────────────────
helper = '''

/// 1.4.1：各模型家族的最长上下文宣称值（全网关统一口径，供模型清单序列化
/// 时覆盖/补齐上游目录缺失值）。匹配按小写包含；未识别的家族返回 None
/// （保留上游原值，不虚构）。
pub fn max_context_window(model: &str) -> Option<u64> {
    let m = model.to_ascii_lowercase();
    // 精确优先：GLM-4.6（Coding Plan）官方 200k，先于 glm 泛匹配
    if m.contains("glm-4.6") {
        return Some(200_000);
    }
    if m.contains("step-5") {
        return Some(1_024_000);
    }
    if m.contains("step") {
        return Some(262_144);
    }
    if m.contains("grok") {
        return Some(500_000);
    }
    if m.contains("minimax") {
        return Some(1_000_000);
    }
    if m.contains("deepseek") || m.contains("hy3") || m.contains("hunyuan") {
        return Some(256_000);
    }
    if m.contains("kimi") || m.contains("moonshot") {
        return Some(256_000);
    }
    // GLM 家族（glm-5.x / glm-4.x / auto / auto-fast / spark）目录实锤 1M
    if m.contains("glm") || m.contains("auto") || m.contains("spark") {
        return Some(1_048_576);
    }
    if m.contains("qwen") {
        return Some(1_048_576);
    }
    None
}
'''
patch('crates/wb-switch-core/src/modules/config.rs',
      [('pub fn store_dir() -> PathBuf {', helper.strip() + '\n\npub fn store_dir() -> PathBuf {')])

# ── 2) glm_proxy.rs：meta_summary 接管 contextWindow ──────────────────────
patch('crates/wb-switch-core/src/modules/glm_proxy.rs',
      [('            "contextWindow": self.meta.get("contextWindow").cloned().unwrap_or(Value::Null),',
        '''            // 1.4.1：全网关统一口径——目录值缺失时用家族最长宣称值兜底
            "contextWindow": crate::modules::config::max_context_window(&self.body_model)
                .map(|v| json!(v))
                .unwrap_or_else(|| self.meta.get("contextWindow").cloned().unwrap_or(Value::Null)),''')])

# ── 3) glmplan.rs：models 条目补 contextWindow ────────────────────────────
patch('crates/wb-switch-core/src/modules/glmplan.rs',
      [('''            data.push(json!({
                "id": model,
                "object": "model",
                "created": 0,
                "owned_by": format!("glmplan:{}", model_provider(cfg, model)),
                "region": region_str,
            }));''',
        '''            let mut entry = json!({
                "id": model,
                "object": "model",
                "created": 0,
                "owned_by": format!("glmplan:{}", model_provider(cfg, model)),
                "region": region_str,
            });
            if let Some(cw) = crate::modules::config::max_context_window(model) {
                entry["contextWindow"] = json!(cw);
            }
            data.push(entry);''')])

# ── 4) glm_web.rs：models 条目补 contextWindow ────────────────────────────
patch('crates/wb-switch-core/src/modules/glm_web.rs',
      [('''    let mut data: Vec<Value> = models
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "chatglm"}))
        .collect::<Vec<Value>>();''',
        '''    let mut data: Vec<Value> = models
        .iter()
        .map(|id| {
            let mut entry = json!({"id": id, "object": "model", "created": 0, "owned_by": "chatglm"});
            if let Some(cw) = crate::modules::config::max_context_window(id) {
                entry["contextWindow"] = json!(cw);
            }
            entry
        })
        .collect::<Vec<Value>>();''')])

# ── 5) workbuddy_upstream.rs：context_length 取 max(家族值, 目录值) ────────
patch('crates/wb-switch-core/src/modules/workbuddy_upstream.rs',
      [('''    if let Some(m) = meta {
        entry.insert("context_length".to_string(), json!(m.context_window));''',
        '''    if let Some(cw) = crate::modules::config::max_context_window(id) {
        entry.insert("context_length".to_string(), json!(cw));
    }
    if let Some(m) = meta {
        if !entry.contains_key("context_length") {
            entry.insert("context_length".to_string(), json!(m.context_window));
        }''')])

print('ALL PATCHES DONE')
