import { createSignal } from "solid-js";
import type { Session, User } from "../lib/types";

const [session, setSession] = createSignal<Session | null>(null);

/** A one-line notice shown on the login screen (e.g. after the session
 * expired). Cleared when the user starts typing a fresh login. */
const [loginNotice, setLoginNotice] = createSignal("");

/** Convenience accessor for the logged-in user (null when logged out). */
function currentUser(): User | null {
  return session()?.user ?? null;
}

export { session, setSession, loginNotice, setLoginNotice, currentUser };
