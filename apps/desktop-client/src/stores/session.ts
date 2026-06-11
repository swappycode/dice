import { createSignal } from "solid-js";
import type { Session, User } from "../lib/types";

const [session, setSession] = createSignal<Session | null>(null);

/** Convenience accessor for the logged-in user (null when logged out). */
function currentUser(): User | null {
  return session()?.user ?? null;
}

export { session, setSession, currentUser };
