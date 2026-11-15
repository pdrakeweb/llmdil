use super::*;
use crate::test_support::*;

#[test]
fn backend_spec_uses_wizard_binary_and_resolves_model() {
    let cfg = test_cfg();
    let b = backend(&cfg, "planner");
    let spec = backend_spec(&cfg, &b, &[]);
    let expected_prog = PathBuf::from("/data")
        .join("llamacpp")
        .join(if cfg!(windows) {
            "llama-server.exe"
        } else {
            "llama-server"
        });
    assert_eq!(spec.program, expected_prog);
    assert_eq!(spec.port, Some(b.port));
    // -m carries the data_dir-resolved model path.
    let pos = spec.args.iter().position(|a| a == "-m").unwrap();
    assert_eq!(
        spec.args[pos + 1],
        PathBuf::from("/data").join(&b.model_file).to_string_lossy()
    );
    // --port carries the configured port.
    let pos = spec.args.iter().position(|a| a == "--port").unwrap();
    assert_eq!(spec.args[pos + 1], b.port.to_string());
}

#[test]
fn backend_spec_split_modes() {
    let cfg = test_cfg();
    for (mode, want) in [
        (manager_config::SplitMode::None, "none"),
        (manager_config::SplitMode::Layer, "layer"),
        (manager_config::SplitMode::Row, "row"),
    ] {
        let mut b = backend(&cfg, "planner");
        b.flags.split_mode = mode;
        let spec = backend_spec(&cfg, &b, &[]);
        let pos = spec.args.iter().position(|a| a == "--split-mode").unwrap();
        assert_eq!(spec.args[pos + 1], want);
    }
}

#[test]
fn backend_spec_tensor_split_joined_with_commas() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.5, 0.3, 0.2];
    let spec = backend_spec(&cfg, &b, &[]);
    let pos = spec
        .args
        .iter()
        .position(|a| a == "--tensor-split")
        .unwrap();
    assert_eq!(spec.args[pos + 1], "0.5,0.3,0.2");
}

fn cuda_visible_devices(spec: &ProcessSpec) -> Option<&str> {
    spec.env
        .iter()
        .find(|(k, _)| k == "CUDA_VISIBLE_DEVICES")
        .map(|(_, v)| v.as_str())
}

fn tensor_split_arg(spec: &ProcessSpec) -> Option<&str> {
    spec.args
        .iter()
        .position(|a| a == "--tensor-split")
        .map(|pos| spec.args[pos + 1].as_str())
}

#[test]
fn backend_spec_unsplit_restricts_to_gpu0() {
    let cfg = test_cfg();
    let b = backend(&cfg, "planner");
    assert!(b.flags.tensor_split.is_empty());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-0"));
    assert_eq!(tensor_split_arg(&spec), None);
}

#[test]
fn backend_spec_single_gpu_on_second_gpu() {
    // Coder's placement: the whole model on GPU 1. The visible
    // set is GPU 1 only (by UUID), and the remapped tensor split
    // is just its proportion — the zero entry for GPU 0 is gone,
    // so llama.cpp never creates its wasted context there.
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.0, 1.0];
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-1"));
    assert_eq!(tensor_split_arg(&spec), Some("1"));
}

#[test]
fn backend_spec_split_across_subset_of_gpus() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.5, 0.0, 0.5];
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-0,GPU-uuid-2"));
    assert_eq!(tensor_split_arg(&spec), Some("0.5,0.5"));
}

#[test]
fn backend_spec_cpu_only_gets_empty_visible_devices() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.5, 0.5];
    b.raw_flags = Some("-ngl 0 --threads 4".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), Some(""));
    assert_eq!(
        tensor_split_arg(&spec),
        None,
        "a split over no visible devices is dropped"
    );
    assert!(spec.args.iter().any(|a| a == "--threads"));
}

#[test]
fn backend_spec_cpu_only_via_gpu_layers_equals() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.raw_flags = Some("--n-gpu-layers=0".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), Some(""));
}

#[test]
fn backend_spec_expert_offload_is_not_cpu_only() {
    // Coder's `-ot exps=CPU` offloads experts only; the rest of
    // the model still loads on the GPU, so the launch restricts
    // to the placement GPU as usual.
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.0, 1.0];
    b.raw_flags = Some("-ot exps=CPU".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-1"));
    assert!(spec.args.iter().any(|a| a == "exps=CPU"));
}

#[test]
fn backend_spec_missing_inventory_gpu_leaves_launch_unrestricted() {
    // GPU 1 fell off the bus before the inventory was taken: its
    // UUID is unknown, so no restriction is guessed at and the
    // configured split passes through verbatim.
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.0, 1.0];
    let gpus = vec![test_gpus().remove(0)];
    let spec = backend_spec(&cfg, &b, &gpus);
    assert_eq!(cuda_visible_devices(&spec), None);
    assert_eq!(tensor_split_arg(&spec), Some("0,1"));
}

#[test]
fn backend_spec_undetermined_split_leaves_launch_unrestricted() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.0, 0.0];
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), None);
    assert_eq!(tensor_split_arg(&spec), Some("0,0"));
}

#[test]
fn backend_spec_unsplit_raw_device_selection_is_not_remapped_to_gpu0() {
    // Code-review 4.4: an unsplit backend whose raw flags carry
    // the placement must launch unrestricted with the flags
    // verbatim — not restricted to GPU 0 with the selection
    // dropped.
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    assert!(b.flags.tensor_split.is_empty());
    b.raw_flags = Some("--main-gpu 1".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), None);
    let pos = spec.args.iter().position(|a| a == "--main-gpu").unwrap();
    assert_eq!(spec.args[pos + 1], "1");

    b.raw_flags = Some("--device CUDA1,CUDA2".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), None);
    assert!(spec.args.iter().any(|a| a == "CUDA1,CUDA2"));

    b.raw_flags = Some("--tensor-split 0,1".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), None);

    // A raw selection of exactly GPU 0 agrees with the unsplit
    // default: the launch restricts as usual.
    b.raw_flags = Some("--main-gpu 0".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(cuda_visible_devices(&spec), Some("GPU-uuid-0"));
}

#[test]
fn raw_flags_pin_cpu_only_is_last_wins() {
    // Code-review 4.7: llama.cpp applies the last occurrence.
    assert!(raw_flags_pin_cpu_only(Some("-ngl 99 -ngl 0")));
    assert!(!raw_flags_pin_cpu_only(Some("-ngl 0 -ngl 99")));
    assert!(raw_flags_pin_cpu_only(Some("-ngl 99 --gpu-layers=0")));
    assert!(!raw_flags_pin_cpu_only(Some("--gpu-layers 0 -ngl 12")));
    assert!(raw_flags_pin_cpu_only(Some("--n-gpu-layers=0")));
    assert!(!raw_flags_pin_cpu_only(Some("-ot exps=CPU")));
    assert!(!raw_flags_pin_cpu_only(None));
}

#[test]
fn backend_spec_remaps_raw_main_gpu_and_drops_conflicts() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.5, 0.0, 0.5];
    // GPU 2 is placed at visible position 1.
    b.raw_flags = Some("--main-gpu 2".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    let pos = spec.args.iter().position(|a| a == "--main-gpu").unwrap();
    assert_eq!(spec.args[pos + 1], "1");
    // GPU 1 is not placed: the flag naming it is dropped.
    b.raw_flags = Some("--main-gpu=1".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert!(!spec.args.iter().any(|a| a.starts_with("--main-gpu")));
}

#[test]
fn backend_spec_raw_tensor_split_yields_to_placement() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.0, 1.0];
    b.raw_flags = Some("--tensor-split 0.5,0.5 --threads 4".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert_eq!(
        spec.args.iter().filter(|a| *a == "--tensor-split").count(),
        1,
        "only the placement's remapped split survives"
    );
    assert_eq!(tensor_split_arg(&spec), Some("1"));
    assert!(spec.args.iter().any(|a| a == "--threads"));
}

#[test]
fn backend_spec_remaps_raw_device_list() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.flags.tensor_split = vec![0.5, 0.0, 0.5];
    b.raw_flags = Some("--device CUDA0,CUDA2".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    let pos = spec.args.iter().position(|a| a == "--device").unwrap();
    assert_eq!(spec.args[pos + 1], "CUDA0,CUDA1");
    // A device outside the placement leaves nothing behind.
    b.raw_flags = Some("--device CUDA1".into());
    let spec = backend_spec(&cfg, &b, &test_gpus());
    assert!(!spec.args.iter().any(|a| a == "--device"));
}

#[test]
fn backend_spec_filters_host_from_raw_flags() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.raw_flags = Some("--host 0.0.0.0 --threads 4".into());
    let spec = backend_spec(&cfg, &b, &[]);
    assert!(!spec.args.iter().any(|a| a == "--host" || a == "0.0.0.0"));
    assert!(spec.args.iter().any(|a| a == "--threads"));
    b.raw_flags = Some("--host=0.0.0.0 --threads 4".into());
    let spec = backend_spec(&cfg, &b, &[]);
    assert!(!spec.args.iter().any(|a| a.starts_with("--host")));
    assert!(spec.args.iter().any(|a| a == "--threads"));
}

#[test]
fn backend_spec_strips_raw_flag_quotes_and_keeps_backslashes() {
    // No shell runs llama-server: a quoted `-ot` value must
    // reach it unquoted (it refused buffer `CPU"` before), with
    // the regex's backslashes intact, exactly like the unquoted
    // spelling.
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    let tail = |spec: &ProcessSpec| {
        let pos = spec.args.iter().position(|a| a == "-ot").unwrap();
        spec.args[pos..].to_vec()
    };
    b.raw_flags = Some(r"-ot blk\..=CPU --alias x".into());
    let plain = tail(&backend_spec(&cfg, &b, &[]));
    assert_eq!(plain, vec!["-ot", r"blk\..=CPU", "--alias", "x"]);
    b.raw_flags = Some(r#"-ot "blk\..=CPU" --alias 'x'"#.into());
    assert_eq!(tail(&backend_spec(&cfg, &b, &[])), plain);
    b.raw_flags = Some(r#"-ot "blk\..=CPU" --alias """#.into());
    assert_eq!(
        tail(&backend_spec(&cfg, &b, &[])),
        vec!["-ot", r"blk\..=CPU", "--alias", ""]
    );
}

#[test]
fn backend_spec_filters_no_jinja_from_raw_flags() {
    // --jinja is app-owned (MCP design §7.1): every raw form
    // that would disable it is stripped, in both spellings,
    // while unrelated raw flags pass through.
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    for raw in [
        "--no-jinja --threads 4",
        "--no_jinja --threads 4",
        "--jinja=false --threads 4",
    ] {
        b.raw_flags = Some(raw.into());
        let spec = backend_spec(&cfg, &b, &[]);
        assert!(
            !spec.args.iter().any(|a| a.starts_with("--no-jinja")
                || a.starts_with("--no_jinja")
                || a == "--jinja=false"),
            "raw form {raw:?} must be filtered"
        );
        assert!(spec.args.iter().any(|a| a == "--threads"));
        // The structured --jinja survives exactly once.
        assert_eq!(spec.args.iter().filter(|a| *a == "--jinja").count(), 1);
    }
}
