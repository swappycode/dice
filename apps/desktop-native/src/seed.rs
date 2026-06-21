//! Milestone-1 seed data — the prototype's `USERS`/`CHAN_GROUPS`/`seed()` data,
//! ported verbatim so the native shell renders the same demo. In milestone 2
//! these models are populated from `Arc<ClientCore>` instead.

use crate::{AppWindow, ChannelRow, ChatMsg, Friend, GuildTile, MemberRow, Reaction, State, VoicePart};
use slint::{ComponentHandle, Color, ModelRc, SharedString, VecModel};

fn hx(s: &str) -> Color {
    let s = s.trim_start_matches('#');
    let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
    Color::from_rgb_u8(r, g, b)
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn s(t: &str) -> SharedString {
    t.into()
}

pub fn apply(ui: &AppWindow) {
    let st = ui.global::<State>();

    // ── guild rail ──
    st.set_guilds(model(vec![
        GuildTile { id: s("baremetal"), label: s("BM"), tint: hx("#5b8cff"), active: true },
        GuildTile { id: s("selfhost"),  label: s("SH"), tint: hx("#36ff8e"), active: false },
        GuildTile { id: s("k8s"),       label: s("K8"), tint: hx("#a78bfa"), active: false },
        GuildTile { id: s("retro"),     label: s("RG"), tint: hx("#ffb454"), active: false },
        GuildTile { id: s("nix"),       label: s("NX"), tint: hx("#46e0ff"), active: false },
    ]));

    // ── channel tree (flattened groups + channels) ──
    let grp = |name: &str| ChannelRow {
        group: true, name: s(name), icon: s(""), voice: false, badge: s(""), active: false,
    };
    let text = |name: &str, badge: &str, active: bool| ChannelRow {
        group: false, name: s(name), icon: s("#"), voice: false, badge: s(badge), active,
    };
    let voice = |name: &str| ChannelRow {
        group: false, name: s(name), icon: s("♪"), voice: true, badge: s(""), active: false,
    };
    st.set_channels(model(vec![
        grp("INFO"),
        text("readme", "", false),
        text("announcements", "2", false),
        grp("LOUNGE"),
        text("general", "", true),
        text("off-topic", "", false),
        text("showcase", "", false),
        grp("HOMELAB"),
        text("hardware", "", false),
        text("networking", "", false),
        text("self-hosting", "5", false),
        text("docker-k8s", "", false),
        grp("VOICE"),
        voice("The Rack"),
        voice("AFK Corner"),
    ]));

    // ── messages (#general) ──
    let no_react: ModelRc<Reaction> = model(vec![]);
    let msg = |author: &str, color: &str, init: &str, time: &str, txt: &str,
               divider: bool, has_reply: bool, reply_name: &str, reply_text: &str,
               reactions: Vec<Reaction>|
     -> ChatMsg {
        let has_reactions = !reactions.is_empty();
        ChatMsg {
            id: s(time),
            author: s(author),
            author_color: hx(color),
            initials: s(init),
            time: s(time),
            text: s(txt),
            edited: false,
            show_header: true,
            grouped: false,
            divider,
            has_reply,
            reply_name: s(reply_name),
            reply_text: s(reply_text),
            has_reactions,
            reactions: if reactions.is_empty() { no_react.clone() } else { model(reactions) },
        }
    };
    let react = |label: &str, me: bool| Reaction { label: s(label), me };

    st.set_messages(model(vec![
        msg("rackmount_randy", "#ffb454", "RR", "2:02 PM",
            "finally got dice self-hosted on the proxmox box. cold start ~1.5s, idle CPU basically a rounding error.",
            false, false, "", "", vec![]),
        msg("sudo_sandra", "#46e0ff", "SS", "2:03 PM",
            "wait it's a pure-rust backend talking QUIC? no JSON on the wire at all??",
            false, false, "", "", vec![]),
        msg("rackmount_randy", "#ffb454", "RR", "2:03 PM",
            "binary protobuf over QUIC, secure-websocket fallback. JSON never touches the realtime path.",
            false, false, "", "", vec![]),
        msg("ECC_or_bust", "#36ff8e", "EC", "2:05 PM",
            "the deploy story is unreal. one monolith binary, flip DICE_SPLIT=1 and it shards into a microservice fleet. no recompile.",
            false, false, "", "", vec![]),
        msg("homelab_hannah", "#ff7ab8", "HH", "2:06 PM",
            "ok but why is it sitting at 117MB idle",
            false, false, "", "", vec![react("😭  3", false)]),
        msg("rackmount_randy", "#ffb454", "RR", "2:07 PM",
            "that's WebView2's floor, not us. a native UI rewrite (iced / slint) is on the roadmap to get under 100.",
            false, false, "", "", vec![]),
        msg("kernelpanic", "#ff5c7a", "KP", "2:11 PM",
            "the aero glass theme on a dark wallpaper goes unreasonably hard",
            true, false, "", "", vec![react("🔥  4", true), react("💯  2", false)]),
        msg("sudo_sandra", "#46e0ff", "SS", "2:12 PM",
            "drop the theme builder, i need the midnight preset immediately",
            false, false, "", "", vec![]),
        msg("fanless_fred", "#5b8cff", "FF", "2:13 PM",
            "pushing my custom theme now — 5 knobs derives the whole palette via color-mix, it's kind of genius",
            false, false, "", "", vec![]),
        msg("ping_flood", "#a78bfa", "PF", "2:15 PM",
            "ThemeBuilder → presets tab, it's built in. midnight is the default now.",
            false, true, "sudo_sandra", "drop the theme builder, i need the midnight preset", vec![]),
    ]));

    // ── member sidebar (flattened) ──
    let hdr = |label: &str, count: i32| MemberRow {
        header: true, label: s(label), count, name: s(""), initials: s(""),
        tint: Color::default(), presence: 0, offline: false, sub: s(""), has_sub: false,
    };
    let mem = |name: &str, init: &str, color: &str, presence: i32, sub: &str, offline: bool| MemberRow {
        header: false, label: s(""), count: 0, name: s(name), initials: s(init),
        tint: hx(color), presence, offline, sub: s(sub), has_sub: true,
    };
    st.set_members(model(vec![
        hdr("Online", 7),
        mem("rackmount_randy", "RR", "#ffb454", 0, "42U and counting", false),
        mem("sudo_sandra", "SS", "#46e0ff", 0, "rm -rf doubts", false),
        mem("ECC_or_bust", "EC", "#36ff8e", 0, "parity matters", false),
        mem("homelab_hannah", "HH", "#ff7ab8", 0, "fiber to the desk", false),
        mem("fanless_fred", "FF", "#5b8cff", 0, "10GbE enjoyer", false),
        mem("kernelpanic", "KP", "#ff5c7a", 2, "compiling…", false),
        mem("ping_flood", "PF", "#a78bfa", 1, "afk-ish", false),
        hdr("Offline", 2),
        mem("quietfan_quinn", "QQ", "#8b93b8", 3, "fanless build", true),
        mem("terabyte_tom", "TT", "#9aa0b5", 3, "48 bays", true),
    ]));

    // ── voice roster (The Rack) ──
    st.set_voice_parts(model(vec![
        VoicePart { name: s("rackmount_randy"), initials: s("RR"), tint: hx("#ffb454"), speaking: true, muted: false },
        VoicePart { name: s("sudo_sandra"), initials: s("SS"), tint: hx("#46e0ff"), speaking: false, muted: false },
        VoicePart { name: s("ECC_or_bust"), initials: s("EC"), tint: hx("#36ff8e"), speaking: false, muted: true },
        VoicePart { name: s("fanless_fred"), initials: s("FF"), tint: hx("#5b8cff"), speaking: false, muted: false },
    ]));

    // ── friends ──
    st.set_friends(model(vec![
        Friend { name: s("rackmount_randy"), initials: s("RR"), tint: hx("#ffb454"), presence: 0, status_text: s("online") },
        Friend { name: s("sudo_sandra"), initials: s("SS"), tint: hx("#46e0ff"), presence: 0, status_text: s("online") },
        Friend { name: s("ECC_or_bust"), initials: s("EC"), tint: hx("#36ff8e"), presence: 0, status_text: s("online") },
        Friend { name: s("homelab_hannah"), initials: s("HH"), tint: hx("#ff7ab8"), presence: 0, status_text: s("online") },
        Friend { name: s("kernelpanic"), initials: s("KP"), tint: hx("#ff5c7a"), presence: 2, status_text: s("do not disturb") },
        Friend { name: s("ping_flood"), initials: s("PF"), tint: hx("#a78bfa"), presence: 1, status_text: s("idle · away") },
    ]));
}
