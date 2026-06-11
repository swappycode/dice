import { createSignal, Show, type Component } from "solid-js";
import { selectedGuildId } from "../../stores/guilds";
import { TitleBar } from "../chrome/TitleBar";
import { StatusBar } from "../chrome/StatusBar";
import { GuildRail } from "../guilds/GuildRail";
import { ChannelTree } from "../channels/ChannelTree";
import { DmList } from "../dm/DmList";
import { ChatView } from "../chat/ChatView";
import { MemberSidebar } from "../chat/MemberSidebar";
import { GuildDialog } from "../dialogs/GuildDialog";
import styles from "./AppShell.module.css";

export const AppShell: Component = () => {
  const [guildDialogOpen, setGuildDialogOpen] = createSignal(false);

  return (
    <div class={styles.window}>
      <TitleBar />
      <div class={styles.body}>
        <GuildRail onAddGuild={() => setGuildDialogOpen(true)} />
        <Show when={selectedGuildId()} fallback={<DmList />}>
          <ChannelTree />
        </Show>
        <ChatView />
        <Show when={selectedGuildId()}>
          <MemberSidebar />
        </Show>
      </div>
      <StatusBar />
      <Show when={guildDialogOpen()}>
        <GuildDialog onClose={() => setGuildDialogOpen(false)} />
      </Show>
    </div>
  );
};
