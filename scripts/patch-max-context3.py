# -*- coding: utf-8 -*-
"""补丁续 2：glm_web（精确锚点）+ workbuddy_upstream。"""
import io

def patch(path, edits):
    t = io.open(path, encoding='utf-8').read()
    for i, (old, new) in enumerate(edits):
        assert old in t, f'{path} anchor#{i} not found: {old[:60]!r}'
        t = t.replace(old, new, 1)
    io.open(path, 'w', encoding='utf-8').write(t)
    print(f'{path}: {len(edits)} edits ok')

patch('crates/wb-switch-core/src/modules/glm_web.rs',
      [('''    let mut data: Vec<Value> = models
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "chatglm"}))
        .collect::<Vec<_>>();''',
        '''    let mut data: Vec<Value> = models
        .iter()
        .map(|id| {
            let mut entry = json!({"id": id, "object": "model", "created": 0, "owned_by": "chatglm"});
            if let Some(cw) = crate::modules::config::max_context_window(id) {
                entry["contextWindow"] = json!(cw);
            }
            entry
        })
        .collect::<Vec<_>>();''')])

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

print('REMAINING PATCHES DONE')
