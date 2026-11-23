// What changes in the mocked control plane, told to the open change streams of its world
// (`GET /v1/changes`): the write handlers say what they changed (a secret, a namespace, a
// workflow, a connection, a script run for the first time), as the server's NOTIFY does.
import type { ControlChange } from "../model/types";
import type { World } from "./fixtures";

type Listener = (change: ControlChange) => void;

const listeners = new WeakMap<World, Set<Listener>>();

/** Tells the open change streams of `world` what changed. */
export function emitChange(world: World, change: ControlChange): void {
  for (const listener of [...(listeners.get(world) ?? [])]) listener(change);
}

/** `response`, and `change` told once it says the write was taken. */
export function told(world: World, change: ControlChange, response: Response): Response {
  if (response.ok) emitChange(world, change);
  return response;
}

/** Follows what changes in `world`, until the answer is called. */
export function onChange(world: World, listener: Listener): () => void {
  let set = listeners.get(world);
  if (!set) {
    set = new Set();
    listeners.set(world, set);
  }
  set.add(listener);
  return () => {
    set?.delete(listener);
  };
}

/** A control-plane change as an event of the stream. */
export function changeEvent(change: ControlChange): string {
  const { kind, ...data } = change;
  return `event: ${kind}\ndata: ${JSON.stringify(data)}\n\n`;
}
