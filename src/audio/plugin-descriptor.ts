/**
 * OWNS: stable frontend identity, scan reconciliation, and picker labels for native plugin
 * descriptors. A plugin class id is only unique inside its file/format; the host loads the tuple.
 */
import type { PluginDescriptor } from '../platform';

type PluginIdentity = Pick<PluginDescriptor, 'format' | 'path' | 'id'>;

/** Match the same identity the native load command receives. */
export function pluginDescriptorKey(desc: PluginIdentity): string {
  return JSON.stringify([desc.format, desc.path, desc.id]);
}

export function samePluginDescriptor(
  left: PluginIdentity | null | undefined,
  right: PluginIdentity | null | undefined,
): boolean {
  return !!left && !!right && pluginDescriptorKey(left) === pluginDescriptorKey(right);
}

/**
 * Keep scan order, collapse exact repeats, then retain loaded descriptors that disappeared from a
 * fresh scan. Descriptors with the same class id at different paths remain separate choices.
 */
export function reconcilePluginDescriptors(
  scanned: readonly PluginDescriptor[],
  loaded: readonly (PluginDescriptor | null)[],
): PluginDescriptor[] {
  const result: PluginDescriptor[] = [];
  const seen = new Set<string>();
  for (const desc of [...scanned, ...loaded]) {
    if (!desc) continue;
    const key = pluginDescriptorKey(desc);
    if (seen.has(key)) continue;
    seen.add(key);
    result.push(desc);
  }
  return result;
}

/** Add a compact location hint only where name + format alone would be ambiguous. */
export function pluginPickerLabel(
  desc: PluginDescriptor,
  peers: readonly PluginDescriptor[],
): string {
  const base = `${desc.name} (${desc.format})`;
  const collisions = peers.filter(
    (peer) =>
      peer.name === desc.name &&
      peer.format === desc.format &&
      !samePluginDescriptor(peer, desc),
  );
  if (collisions.length === 0) return base;

  const segments = desc.path.split(/[\\/]/).filter(Boolean);
  const parent = segments.at(-2) ?? desc.path;
  const parentIsUnique = collisions.every((peer) => {
    const peerSegments = peer.path.split(/[\\/]/).filter(Boolean);
    return (peerSegments.at(-2) ?? peer.path) !== parent;
  });
  return `${base} · ${parentIsUnique ? parent : desc.path}`;
}
