// rewind:internal (epoch 1): receives the hooks from the bootstrap. Only the runtime's own modules
// may import it.
const hooks = globalThis.__rewindHandoff;
delete globalThis.__rewindHandoff;
export default hooks;
