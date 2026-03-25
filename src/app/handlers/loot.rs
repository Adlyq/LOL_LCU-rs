use tracing::{debug, error, info};
use windows::Win32::Foundation::HWND;

use crate::lcu::api::LcuClient;

pub async fn handle_find_forgotten_loot(api: LcuClient) {
    let loot_list = match api.get_player_loot().await {
        Ok(v) => v,
        Err(e) => {
            error!("获取战利品失败: {e}");
            return;
        }
    };

    let Some(loots) = loot_list.as_array() else {
        return;
    };
    let mut claimable = Vec::new();

    for loot in loots {
        let loot_id = loot.get("lootId").and_then(|v| v.as_str()).unwrap_or("");
        let count = loot.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        let loot_name = loot
            .get("localizedName")
            .and_then(|v| v.as_str())
            .or_else(|| loot.get("localizedDescription").and_then(|v| v.as_str()))
            .unwrap_or(loot_id);

        if count <= 0 {
            continue;
        }

        // 识别逻辑：匹配常见的可领取奖励前缀
        // 参考 Akari: CURRENCY_champion_faceoff, REWARD_..., CHEST_...
        let is_reward = loot_id.starts_with("REWARD_")
            || loot_id.contains("champion_faceoff")
            || loot_id.starts_with("CHEST_")
            || loot_id.contains("_REWARD");

        if is_reward {
            // 尝试寻找配方：Akari 常用的是 CHEST_generic_OPEN 或 REWARD_claim
            let recipe = if loot_id.starts_with("CHEST_") {
                "CHEST_generic_OPEN"
            } else {
                "REWARD_claim"
            };
            claimable.push((
                loot_id.to_owned(),
                loot_name.to_owned(),
                recipe.to_owned(),
                count,
            ));
        }
    }

    if claimable.is_empty() {
        unsafe {
            use windows::core::PCWSTR;
            use windows::Win32::UI::WindowsAndMessaging::*;
            let text = crate::win::winapi::to_wide("没有发现可领取的遗忘资源。");
            let caption = crate::win::winapi::to_wide("战利品检查");
            MessageBoxW(
                HWND::default(),
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
            );
        }
        return;
    }

    let mut list_str = String::new();
    for (_, name, _, count) in &claimable {
        list_str.push_str(&format!(" - {} (数量: {})\n", name, count));
    }

    let msg = format!("发现以下可领取资源：\n\n{}\n是否立即找回？", list_str);

    let confirm = unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let text = crate::win::winapi::to_wide(&msg);
        let caption = crate::win::winapi::to_wide("找回遗忘的东西");
        let res = MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OKCANCEL | MB_ICONQUESTION | MB_SETFOREGROUND,
        );
        res == IDOK
    };

    if confirm {
        info!("正在开始找回战利品...");
        for (id, name, recipe, _) in claimable {
            debug!("正在领取: {} (ID: {}, 配方: {})", name, id, recipe);
            let _ = api.call_loot_recipe(&id, &recipe).await;
        }
        info!("找回任务执行完毕。");
    }
}
