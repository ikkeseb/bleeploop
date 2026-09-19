/** OWNS: the shared capture-ring packet format. Timestamp and PCM publish in one atomic ring push. */
export const CAPTURE_QUANTUM_FRAMES = 128;
export const CAPTURE_PACKET_HEADER = 2;
export const CAPTURE_PACKET_SIZE = CAPTURE_PACKET_HEADER + CAPTURE_QUANTUM_FRAMES;

/** Float64 retains exact absolute frame timestamps for long sessions and losslessly holds Float32 PCM. */
export function capturePacketCapacity(audioFrames: number): number {
  return Math.ceil(audioFrames / CAPTURE_QUANTUM_FRAMES) * CAPTURE_PACKET_SIZE;
}
