import { type Component, createMemo, createResource, Show } from "solid-js";
import { ipc } from "../../lib/ipc";
import styles from "./Avatar.module.css";

export function initialsOf(name: string): string {
  const parts = name.trim().split(/[\s_\-.]+/).filter(Boolean);
  const first = parts[0]?.[0] ?? "?";
  const second = parts.length > 1 ? (parts[parts.length - 1]?.[0] ?? "") : (parts[0]?.[1] ?? "");
  return (first + second).toUpperCase();
}

/** Initials tile, or the user's avatar image when they have one (avatars are
 *  media; bytes resolve via the same `ipc.attachmentSrc` path as attachments). */
export const Avatar: Component<{ name: string; avatarId?: string | null; size?: "sm" | "md" }> = (
  props,
) => {
  const initials = createMemo(() => initialsOf(props.name));
  const [src] = createResource(
    () => props.avatarId ?? null,
    (id) => ipc.attachmentSrc(id),
  );
  const sizeClass = () => (props.size === "sm" ? styles.sm : "");
  return (
    <Show
      when={props.avatarId && src()}
      fallback={
        <span class={`${styles.avatar} ${sizeClass()}`} aria-hidden="true">
          {initials()}
        </span>
      }
    >
      <img
        class={`${styles.avatar} ${styles.img} ${sizeClass()}`}
        src={src() ?? ""}
        alt={props.name}
        title={props.name}
      />
    </Show>
  );
};
