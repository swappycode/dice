import type { Component } from "solid-js";
import type { PresenceStatus } from "../../lib/types";

/** Glossy XP presence orb — consumes the .orb recipe (recipes.css). */
export const PresenceOrb: Component<{ status: PresenceStatus; title?: string }> = (props) => (
  <span
    class={`orb orb--${props.status}`}
    role="img"
    aria-label={props.status}
    title={props.title ?? props.status}
  />
);
