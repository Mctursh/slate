use slate_replay::{ReplayBank, build_feature_set, store::DiskStore};
use solana_program_runtime::solana_sbpf::{ebpf, elf::Executable, verifier::RequisiteVerifier};
use solana_svm_feature_set::SVMFeatureSet;

const NAMES: &[&str] = &[
    "abort",
    "sol_panic_",
    "sol_get_clock_sysvar",
    "sol_get_rent_sysvar",
    "sol_get_return_data",
    "sol_invoke_signed_rust",
    "sol_log_",
    "sol_log_data",
    "sol_log_pubkey",
    "sol_memcmp_",
    "sol_memcpy_",
    "sol_memmove_",
    "sol_memset_",
    "sol_sha256",
    "sol_try_find_program_address",
];

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: <accounts.redb> <slot> [elf]");
    let slot: u64 = args.next().expect("slot").parse()?;
    let elf_path = args.next();

    let store = DiskStore::open(&path, 1 << 28)?;
    let bank = ReplayBank::with_store(Box::new(store));
    let fs = build_feature_set(&bank, slot);
    let svm: SVMFeatureSet = fs.runtime_features();

    let loader = agave_syscalls::create_program_runtime_environment_v1(
        &svm,
        &solana_program_runtime::execution_budget::SVMTransactionExecutionBudget::default(),
        false,
        false,
    )
    .expect("env");

    let cfg = loader.get_config();
    println!("enabled_sbpf_versions = {:?}", cfg.enabled_sbpf_versions);
    println!("reject_broken_elfs    = {}", cfg.reject_broken_elfs);
    println!("aligned_memory_mapping= {}", cfg.aligned_memory_mapping);
    println!("max_call_depth        = {}", cfg.max_call_depth);
    println!("stack_frame_size      = {}", cfg.stack_frame_size);
    println!();
    for n in NAMES {
        let h = ebpf::hash_symbol_name(n.as_bytes());
        let found = loader.get_function_registry().lookup_by_key(h).is_some();
        println!(
            "{:<30} hash={:<12} {}",
            n,
            h,
            if found { "ok" } else { "MISSING" }
        );
    }
    let Some(elf_path) = elf_path else {
        return Ok(());
    };
    let bytes = std::fs::read(&elf_path)?;
    let loader = std::sync::Arc::new(loader);
    match Executable::load(&bytes, loader.clone()) {
        Err(e) => println!("\nload FAILED: {e:?}"),
        Ok(exe) => {
            println!("\nloaded: sbpf_version {:?}", exe.get_sbpf_version());
            match exe.verify::<RequisiteVerifier>() {
                Err(e) => println!("verify FAILED: {e:?}"),
                Ok(()) => println!("verify ok"),
            }
            let (_, text) = exe.get_text_bytes();
            let syscalls = loader.get_function_registry();
            let internal = exe.get_function_registry();
            let mut bad = 0usize;
            let n = text.len() / ebpf::INSN_SIZE;
            for pc in 0..n {
                let insn = ebpf::get_insn(text, pc);
                if insn.opc != ebpf::CALL_IMM {
                    continue;
                }
                let key = exe
                    .get_sbpf_version()
                    .calculate_call_imm_target_pc(pc, insn.imm);
                if syscalls.lookup_by_key(insn.imm as u32).is_none()
                    && internal.lookup_by_key(key).is_none()
                {
                    bad += 1;
                    if bad <= 20 {
                        println!("  UNRESOLVED call at pc {pc} imm {} (key {key})", insn.imm);
                    }
                }
            }
            println!("instructions {n}, unresolved call imm: {bad}");
            let (addr, _) = exe.get_text_bytes();
            println!("text vaddr {addr} len {}", text.len());
            println!("ro_section len {}", exe.get_ro_section().len());
            let mut fr: Vec<(u32, usize)> = internal.iter().map(|(k, (_, pc))| (k, pc)).collect();
            fr.sort();
            println!("internal functions {}", fr.len());
            if let Ok(out) = std::env::var("DUMP") {
                let mut s = String::new();
                for (k, pc) in &fr {
                    s.push_str(&format!("{k} {pc}\n"));
                }
                std::fs::write(format!("{out}.registry"), s).unwrap();
                std::fs::write(format!("{out}.text"), text).unwrap();
                std::fs::write(format!("{out}.ro"), exe.get_ro_section()).unwrap();
            }
            let mut hist = std::collections::BTreeMap::<u8, usize>::new();
            let mut pc = 0usize;
            while pc < n {
                let insn = ebpf::get_insn(text, pc);
                *hist.entry(insn.opc).or_default() += 1;
                pc += if insn.opc == ebpf::LD_DW_IMM { 2 } else { 1 };
            }
            println!("distinct opcodes: {}", hist.len());
            for (opc, c) in &hist {
                println!("  0x{opc:02x}  {c}");
            }
        }
    }
    Ok(())
}
