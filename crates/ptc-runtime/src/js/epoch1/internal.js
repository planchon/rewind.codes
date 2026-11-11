// ptc:internal (epoch 1): receives the hooks from the bootstrap. Only the runtime's own modules
// may import it.
const hooks = globalThis.__ptcHandoff;
delete globalThis.__ptcHandoff;
export default hooks;
