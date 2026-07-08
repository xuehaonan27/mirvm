//! 加载相：MIR → M4 引擎字节码的降低（rustc_private 域，tcx 关在这里）。
//!
//! 编排（M4.0 设计 §3）：mono 收集（与 native codegen 同一套 instance 集，D1）→
//! 逐 instance 降低（frame 布局冻结 + 语句翻译）→ 产出纯 Rust 的 `ir::Module`。
//!
//! **Trap-stub 全覆盖**：对收集全集 lowering 是全量的——不认识的构造绝不中止，
//! 就地降为 `Trap(诊断)`；只有被执行到的路径必须 trap-free（M4 增量协议）。

pub mod collect;
pub mod frame;
pub mod func;

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::ty::{Instance, TyCtxt, TypingEnv};

use crate::vm::engine::ir;

/// 整程序降低：收集 → 分配 FuncId → 逐 instance 翻译。
pub fn lower_program(tcx: TyCtxt<'_>) -> ir::Module {
    let typing_env = TypingEnv::fully_monomorphized();
    let instances = collect::collect(tcx);

    // 预分配 instance → FuncId（调用点解析用）
    let mut ids: FxHashMap<Instance<'_>, ir::FuncId> = FxHashMap::default();
    for (i, inst) in instances.iter().enumerate() {
        ids.insert(*inst, i as ir::FuncId);
    }

    let mut module = ir::Module::default();
    for (i, inst) in instances.iter().enumerate() {
        let sym = tcx.symbol_name(*inst).name.to_owned();
        let body = func::lower_instance(tcx, typing_env, *inst, &ids)
            .unwrap_or_else(|reason| func::trap_body(&sym, &reason));
        module.exports.insert(sym.into_boxed_str(), i as ir::FuncId);
        module.funcs.push(body);
    }
    module
}
