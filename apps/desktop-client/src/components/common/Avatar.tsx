import { type Component, createMemo } from "solid-js";
import styles from "./Avatar.module.css";

export function initialsOf(name: string): string {
  const parts = name.trim().split(/[\s_\-.]+/).filter(Boolean);
  const first = parts[0]?.[0] ?? "?";
  const second = parts.length > 1 ? (parts[parts.length - 1]?.[0] ?? "") : (parts[0]?.[1] ?? "");
  return (first + second).toUpperCase();
}

/** Initials tile — no raster images anywhere in the app. */
export const Avatar: Component<{ name: string; size?: "sm" | "md" }> = (props) => {
  const initials = createMemo(() => initialsOf(props.name));
  return (
    <span class={`${styles.avatar} ${props.size === "sm" ? styles.sm : ""}`} aria-hidden="true">
      {initials()}
    </span>
  );
};
