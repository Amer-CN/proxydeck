//! 注水检测的**渠道注册表（静态面）** —— `internal/tuanjie/channels.go` 的对应物（P8b 切片）。
//!
//! 本模块只搬 `channels.go` 里不依赖网络/文件的部分：`waterChannels`（`:40-49`）、
//! `isStrongChannel`（`:57-59`）、`channelNameOf`（`:133-143`）。**网络面与密钥配置面
//! 不在本切片**（`buildChannels`/`fetchLocalModels`/`channelTarget` 走 HTTP，
//! `LoadChannelKeys`/`SaveChannelKey` 走 `appdir.ReadFile/WriteFile` 的 DPAPI 落盘）。

/// 渠道静态定义 —— `waterChannelDef`（`channels.go:31-37`）。
///
/// `bearer_from`：`""` | `"config"` | `"api-key.txt"`；`port == 0` 表示
/// 「本地端口字段对它无意义」（远端 BaseURL 渠道）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaterChannelDef {
    pub id: &'static str,
    pub name: &'static str,
    pub port: u16,
    pub base_url: &'static str,
    pub bearer_from: &'static str,
}

/// 内置八渠道（**顺序即 channels 列表顺序**）—— `waterChannels`（`channels.go:40-49`）。
pub const WATER_CHANNELS: [WaterChannelDef; 8] = [
    WaterChannelDef {
        id: "tuanjie",
        name: "团结",
        port: 8788,
        base_url: "",
        bearer_from: "",
    },
    WaterChannelDef {
        id: "command",
        name: "Command",
        port: 55990,
        base_url: "http://127.0.0.1:55990",
        bearer_from: "api-key.txt",
    },
    WaterChannelDef {
        id: "workbuddy",
        name: "WorkBuddy",
        port: 8787,
        base_url: "http://127.0.0.1:8787",
        bearer_from: "",
    },
    WaterChannelDef {
        id: "workbuddy-intl",
        name: "WorkBuddy Intl",
        port: 8789,
        base_url: "http://127.0.0.1:8789",
        bearer_from: "",
    },
    WaterChannelDef {
        id: "bai",
        name: "B.ai",
        port: 8891,
        base_url: "http://127.0.0.1:8891",
        bearer_from: "config",
    },
    WaterChannelDef {
        id: "comate",
        name: "Comate",
        port: 8786,
        base_url: "http://127.0.0.1:8786",
        bearer_from: "",
    },
    WaterChannelDef {
        id: "qoder",
        name: "Qoder",
        port: 8785,
        base_url: "http://127.0.0.1:8785",
        bearer_from: "",
    },
    WaterChannelDef {
        id: "tokenrouter",
        name: "TokenRouter",
        port: 0,
        base_url: "https://api.tokenrouter.com",
        bearer_from: "config",
    },
];

/// 渠道密钥配置文件名 —— `channels.go:79-81` 的 `channelsConfigPath()` 末段。
pub const CHANNELS_CONFIG_FILE: &str = "tuanjie-water-channels.json";

/// 官方链路渠道判定 —— `isStrongChannel`（`channels.go:57-59`）。
pub fn is_strong_channel(id: &str) -> bool {
    matches!(id, "tuanjie" | "comate" | "qoder" | "tokenrouter")
}

/// 渠道 id → 人话名（未命中原样返回；空串回「团结」）—— `channelNameOf`
/// （`channels.go:133-143`）。
pub fn channel_name_of(channel: &str) -> &str {
    for c in WATER_CHANNELS.iter() {
        if c.id == channel {
            return c.name;
        }
    }
    if channel.is_empty() || channel == "tuanjie" {
        return "团结";
    }
    channel
}

/// 按 id 取渠道定义 —— `channelTarget`（`channels.go:284-290`）的查表段。
pub fn channel_def(id: &str) -> Option<&'static WaterChannelDef> {
    WATER_CHANNELS.iter().find(|c| c.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_and_name_tables_match_go() {
        for (id, want) in [
            ("tuanjie", true),
            ("comate", true),
            ("qoder", true),
            ("tokenrouter", true),
            ("command", false),
            ("workbuddy", false),
            ("bai", false),
            ("", false),
            ("unknown", false),
        ] {
            assert_eq!(is_strong_channel(id), want, "isStrongChannel({id:?})");
        }
        for (id, want) in [
            ("", "团结"),
            ("tuanjie", "团结"),
            ("command", "Command"),
            ("workbuddy", "WorkBuddy"),
            ("workbuddy-intl", "WorkBuddy Intl"),
            ("bai", "B.ai"),
            ("comate", "Comate"),
            ("qoder", "Qoder"),
            ("tokenrouter", "TokenRouter"),
            ("unknown", "unknown"),
            ("BAI", "BAI"),
        ] {
            assert_eq!(channel_name_of(id), want, "channelNameOf({id:?})");
        }
    }

    #[test]
    fn channel_table_order_and_fields_match_go() {
        let ids: Vec<&str> = WATER_CHANNELS.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![
                "tuanjie",
                "command",
                "workbuddy",
                "workbuddy-intl",
                "bai",
                "comate",
                "qoder",
                "tokenrouter"
            ]
        );
        assert_eq!(channel_def("tokenrouter").unwrap().port, 0, "远端渠道端口留零值");
        assert_eq!(channel_def("tuanjie").unwrap().base_url, "", "团结走账号池");
    }
}
