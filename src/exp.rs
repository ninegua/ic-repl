use super::error::pretty_parse;
use super::helper::{find_init_args, MyHelper, OfflineOutput};
use super::selector::{project, Selector};
use super::token::{ParserError, Tokenizer};
use super::utils::{
    args_to_value, as_u32, cast_type, get_effective_canister_id, get_field, resolve_path,
    str_to_principal,
};
use anyhow::{anyhow, Context, Result};
use candid::{
    types::value::{IDLArgs, IDLField, IDLValue, VariantValue},
    types::{Function, Label, Type, TypeInner},
    utils::check_unique,
    Principal, TypeEnv,
};
use futures::future::try_join_all;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Exp {
    Path(String, Vec<Selector>),
    AnnVal(Box<Exp>, Type),
    Call {
        method: Option<Method>,
        args: Option<Vec<Exp>>,
        mode: CallMode,
    },
    ParCall {
        calls: Vec<FuncCall>,
    },
    Decode {
        method: Option<Method>,
        blob: Box<Exp>,
    },
    Apply(String, Vec<Exp>),
    Fail(Box<Exp>),
    // from IDLValue without the infered types
    Bool(bool),
    Null,
    Text(String),
    Number(String), // Undetermined number type
    Float64(f64),
    Opt(Box<Exp>),
    Blob(Vec<u8>),
    Vec(Vec<Exp>),
    Record(Vec<Field>),
    Variant(Box<Field>, u64), // u64 represents the index from the type, defaults to 0 when parsing
    Principal(Principal),
    Service(Principal),
    Func(Principal, String),
}
#[derive(Debug, Clone)]
pub struct Method {
    pub canister: String,
    pub method: String,
}
#[derive(Debug, Clone)]
pub enum CallMode {
    Call,
    Encode,
    Proxy(String),
}
#[derive(Debug, Clone)]
pub struct FuncCall {
    pub method: Method,
    pub args: Vec<Exp>,
}
#[derive(Debug, Clone)]
pub struct Field {
    pub id: Label,
    pub val: Exp,
}
impl Exp {
    pub fn is_call(&self) -> bool {
        // Used to decide if we want to report profiling numbers. Ignore par_call for now
        matches!(
            self,
            Exp::Call {
                mode: CallMode::Call,
                ..
            }
        )
    }
    pub fn eval(self, helper: &MyHelper) -> Result<IDLValue> {
        Ok(match self {
            Exp::Path(id, path) => {
                let v = helper
                    .env
                    .0
                    .get(&id)
                    .ok_or_else(|| anyhow!("Undefined variable {}", id))?
                    .clone();
                project(helper, v, path)?
            }
            Exp::AnnVal(v, ty) => {
                let arg = v.eval(helper)?;
                cast_type(arg, &ty).with_context(|| format!("casting to type {ty} fails"))?
            }
            Exp::Fail(v) => match v.eval(helper) {
                Err(e) => IDLValue::Text(e.to_string()),
                Ok(_) => return Err(anyhow!("Expects an error state")),
            },
            Exp::Apply(func, exps) => {
                use crate::account_identifier::*;

                // functions that cannot evaluate arguments first
                match func.as_str() {
                    "ite" => {
                        if exps.len() != 3 {
                            return Err(anyhow!(
                                "ite expects a bool, true branch and false branch"
                            ));
                        }
                        return Ok(match exps[0].clone().eval(helper)? {
                            IDLValue::Bool(true) => exps[1].clone().eval(helper)?,
                            IDLValue::Bool(false) => exps[2].clone().eval(helper)?,
                            _ => {
                                return Err(anyhow!(
                                    "ite expects the first argument to be a boolean expression"
                                ));
                            }
                        });
                    }
                    "exist" => {
                        if exps.len() != 1 {
                            return Err(anyhow!("exist expects an expression"));
                        }
                        return Ok(match exps[0].clone().eval(helper) {
                            Ok(_) => IDLValue::Bool(true),
                            Err(_) => IDLValue::Bool(false),
                        });
                    }
                    "export" => {
                        use std::io::{BufWriter, Write};
                        if exps.len() <= 1 {
                            return Err(anyhow!("export expects at least two arguments"));
                        }
                        let path = exps[0].clone().eval(helper)?;
                        let IDLValue::Text(path) = path else {
                            return Err(anyhow!("export expects first argument to be a file path"));
                        };
                        let path = resolve_path(&std::env::current_dir()?, &path);
                        let file = std::fs::File::create(path)?;
                        let mut writer = BufWriter::new(file);
                        for arg in exps.iter().skip(1) {
                            let Exp::Path(id, _) = arg else {
                                return Err(anyhow!("export expects variables"));
                            };
                            let val = arg.clone().eval(helper)?;
                            writeln!(&mut writer, "let {id} = {val};")?;
                        }
                        return Ok(IDLValue::Null);
                    }
                    _ => (),
                }

                let mut args = Vec::new();
                for e in exps.into_iter() {
                    args.push(e.eval(helper)?);
                }
                match func.as_str() {
                    "account" => match args.as_slice() {
                        [IDLValue::Principal(principal)] => {
                            let account = AccountIdentifier::new(*principal, None);
                            IDLValue::Blob(account.to_vec())
                        }
                        [IDLValue::Principal(principal), IDLValue::Blob(subaccount)] => {
                            let subaccount = Subaccount::try_from(subaccount.as_slice())?;
                            let account = AccountIdentifier::new(*principal, Some(subaccount));
                            IDLValue::Blob(account.to_vec())
                        }
                        _ => return Err(anyhow!("account expects principal")),
                    },
                    "subaccount" => match args.as_slice() {
                        [IDLValue::Principal(principal)] => {
                            let subaccount = Subaccount::from(principal);
                            IDLValue::Blob(subaccount.to_vec())
                        }
                        _ => return Err(anyhow!("account expects principal")),
                    },
                    "neuron_account" => match args.as_slice() {
                        [IDLValue::Principal(principal), nonce] => {
                            let nonce = match nonce {
                                IDLValue::Number(nonce) => nonce.parse::<u64>()?,
                                IDLValue::Nat64(nonce) => *nonce,
                                _ => {
                                    return Err(anyhow!(
                                        "neuron_account expects (principal, nonce)"
                                    ))
                                }
                            };
                            let nns = Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai")?;
                            let subaccount = get_neuron_subaccount(principal, nonce);
                            let account = AccountIdentifier::new(nns, Some(subaccount));
                            IDLValue::Blob(account.to_vec())
                        }
                        _ => return Err(anyhow!("neuron_account expects (principal, nonce)")),
                    },
                    "replica_url" => match args.as_slice() {
                        [] => IDLValue::Text(helper.agent_url.clone()),
                        _ => return Err(anyhow!("replica_url expects no arguments")),
                    },
                    "read_state" if helper.offline.is_none() => {
                        use crate::utils::{fetch_state_path, parse_state_path};
                        match args.as_slice() {
                            [IDLValue::Text(_), ..] => {
                                let path = parse_state_path(args.as_slice())?;
                                fetch_state_path(&helper.agent, path)?
                            }
                            [IDLValue::Principal(effective), IDLValue::Text(_), ..] => {
                                let mut path = parse_state_path(&args[1..])?;
                                path.effective_id = Some(*effective);
                                fetch_state_path(&helper.agent, path)?
                            }
                            _ => {
                                return Err(anyhow!(
                                "read_state expects ([effective_id,] prefix, principal, path, ...)"
                            ))
                            }
                        }
                    }
                    "file" => match args.as_slice() {
                        [IDLValue::Text(file)] => {
                            let path = resolve_path(&helper.base_path, file);
                            IDLValue::Blob(
                                std::fs::read(&path)
                                    .with_context(|| format!("Cannot read {path:?}"))?,
                            )
                        }
                        _ => return Err(anyhow!("file expects file path")),
                    },
                    "gzip" => match args.as_slice() {
                        [IDLValue::Blob(blob)] => {
                            use libflate::gzip::Encoder;
                            use std::io::Write;
                            let mut encoder = Encoder::new(Vec::with_capacity(blob.len()))?;
                            encoder.write_all(blob)?;
                            let result = encoder.finish().into_result()?;
                            IDLValue::Blob(result)
                        }
                        _ => return Err(anyhow!("gzip expects blob")),
                    },
                    "exec" => match args.as_slice() {
                        [IDLValue::Text(cmd), ..] => {
                            use std::io::{BufRead, BufReader};
                            use std::process::{Command, Stdio};
                            use std::sync::{Arc, Mutex};
                            let mut cmd = Command::new(cmd);
                            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                            let mut is_silence = false;
                            let mut cwd = None;
                            let n = args.len();
                            for (i, arg) in args.iter().skip(1).enumerate() {
                                match arg {
                                    IDLValue::Text(arg) => {
                                        cmd.arg(arg);
                                    }
                                    IDLValue::Record(fs) if i == n - 2 => {
                                        if let Some(v) = get_field(fs, "cwd") {
                                            if let IDLValue::Text(path) = v {
                                                cwd = Some(resolve_path(&helper.base_path, path));
                                            } else {
                                                return Err(anyhow!("cwd expects a string"));
                                            }
                                        }
                                        if let Some(v) = get_field(fs, "silence") {
                                            if let IDLValue::Bool(silence) = v {
                                                is_silence = *silence;
                                            } else {
                                                return Err(anyhow!("silence expects a boolean"));
                                            }
                                        }
                                    }
                                    _ => return Err(anyhow!("exec expects string arguments")),
                                }
                            }
                            if let Some(cwd) = cwd {
                                cmd.current_dir(cwd);
                            }
                            let mut child = cmd.spawn()?;
                            let stdout = child.stdout.take().unwrap();
                            let stderr = child.stderr.take().unwrap();
                            let final_stdout = Arc::new(Mutex::new(String::new()));
                            let final_stdout_clone = Arc::clone(&final_stdout);

                            let stdout_thread = std::thread::spawn(move || {
                                let reader = BufReader::new(stdout);
                                reader.lines().for_each(|line| {
                                    if let Ok(line) = line {
                                        if !is_silence {
                                            println!("{line}");
                                        }
                                        let mut final_stdout = final_stdout_clone.lock().unwrap();
                                        *final_stdout = line;
                                    }
                                });
                            });
                            let mut stderr_thread = None;
                            if !is_silence {
                                stderr_thread = Some(std::thread::spawn(move || {
                                    let reader = BufReader::new(stderr);
                                    reader.lines().for_each(|line| {
                                        if let Ok(line) = line {
                                            eprintln!("{line}");
                                        }
                                    });
                                }));
                            }
                            let status = child.wait()?;
                            stdout_thread.join().unwrap();
                            if let Some(thread) = stderr_thread {
                                thread.join().unwrap();
                            }
                            if !status.success() {
                                return Err(anyhow!(
                                    "exec failed with status {}",
                                    status.code().unwrap_or(-1)
                                ));
                            }
                            let stdout = final_stdout.lock().unwrap();
                            candid_parser::parse_idl_value(&stdout)
                                .unwrap_or(IDLValue::Text(stdout.clone()))
                        }
                        _ => return Err(anyhow!("exec expects (text command, ...text args)")),
                    },
                    "send" if helper.offline.is_none() => match args.as_slice() {
                        [IDLValue::Blob(blob)] => {
                            use crate::offline::{send, send_messages};
                            let json = std::str::from_utf8(blob)?;
                            let res = match json.trim_start().chars().next() {
                                Some('{') => send(helper, &serde_json::from_str(json)?)?,
                                Some('[') => send_messages(helper, &serde_json::from_str(json)?)?,
                                _ => return Err(anyhow!("not a valid json message")),
                            };
                            args_to_value(res)
                        }
                        _ => return Err(anyhow!("send expects a json blob")),
                    },
                    "wasm_profiling" => match args.as_slice() {
                        [IDLValue::Text(file)] | [IDLValue::Text(file), IDLValue::Record(_)] => {
                            use ic_wasm::instrumentation::{instrument, Config};
                            let path = resolve_path(&helper.base_path, file);
                            let blob = std::fs::read(&path)
                                .with_context(|| format!("Cannot read {path:?}"))?;
                            let mut m = ic_wasm::utils::parse_wasm(&blob, false)?;
                            ic_wasm::shrink::shrink(&mut m);
                            let config = match args.get(1) {
                                Some(IDLValue::Record(fs)) => {
                                    let start_page = if let Some(n) = get_field(fs, "start_page") {
                                        Some(as_u32(n).with_context(|| {
                                            anyhow!("start_page expects a number")
                                        })? as i32)
                                    } else {
                                        None
                                    };
                                    let page_limit = if start_page.is_some() {
                                        if let Some(n) = get_field(fs, "page_limit") {
                                            Some(as_u32(n).with_context(|| {
                                                anyhow!("page_limit expects a number")
                                            })?
                                                as i32)
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    };
                                    let trace_only_funcs = if let Some(v) =
                                        get_field(fs, "trace_only_funcs")
                                    {
                                        if let IDLValue::Vec(vec) = v {
                                            vec.iter()
                                                .filter_map(|name| {
                                                    if let IDLValue::Text(name) = name {
                                                        Some(name.clone())
                                                    } else {
                                                        None
                                                    }
                                                })
                                                .collect()
                                        } else {
                                            return Err(anyhow!("trace_only_funcs expects a vetor of function names"));
                                        }
                                    } else {
                                        vec![]
                                    };
                                    Config {
                                        trace_only_funcs,
                                        start_address: start_page
                                            .map(|page| i64::from(page) * 65536),
                                        page_limit,
                                    }
                                }
                                Some(_) => unreachable!(),
                                None => Config {
                                    trace_only_funcs: vec![],
                                    start_address: None,
                                    page_limit: None,
                                },
                            };
                            instrument(&mut m, config).map_err(|e| anyhow::anyhow!("{e}"))?;
                            IDLValue::Blob(m.emit_wasm())
                        }
                        _ => {
                            return Err(anyhow!(
                                "wasm_profiling expects file path and optionally record for config"
                            ))
                        }
                    },
                    "flamegraph" => match args.as_slice() {
                        [IDLValue::Principal(cid), IDLValue::Text(title), IDLValue::Text(file)] => {
                            let mut map = helper.canister_map.borrow_mut();
                            let names = match map.get(&helper.agent, cid) {
                                Ok(crate::helper::CanisterInfo {
                                    profiling: Some(names),
                                    ..
                                }) => names,
                                _ => return Err(anyhow!("{} is not instrumented", cid)),
                            };
                            let mut path = resolve_path(&std::env::current_dir()?, file);
                            if path.extension().is_none() {
                                path.set_extension("svg");
                            }
                            let cost = crate::profiling::get_profiling(
                                &helper.agent,
                                cid,
                                names,
                                title,
                                path,
                            )?;
                            IDLValue::Nat(cost.into())
                        }
                        _ => {
                            return Err(anyhow!(
                                "flamegraph expects (canister id, title name, svg file name)"
                            ))
                        }
                    },
                    "output" => match args.as_slice() {
                        [IDLValue::Text(file), IDLValue::Text(content)] => {
                            use std::fs::OpenOptions;
                            use std::io::Write;
                            let path = resolve_path(&std::env::current_dir()?, file);
                            let mut file =
                                OpenOptions::new().append(true).create(true).open(path)?;
                            file.write_all(content.as_bytes())?;
                            IDLValue::Text(content.to_string())
                        }
                        _ => return Err(anyhow!("wasm_profiling expects (file path, content)")),
                    },
                    "stringify" => {
                        use std::fmt::Write;
                        let mut res = String::new();
                        for arg in args {
                            write!(&mut res, "{}", crate::utils::stringify(&arg)?)?;
                        }
                        IDLValue::Text(res)
                    }
                    "concat" => match args.as_slice() {
                        [IDLValue::Vec(s1), IDLValue::Vec(s2)] => {
                            let mut res = Vec::from(s1.as_slice());
                            res.extend_from_slice(s2);
                            IDLValue::Vec(res)
                        }
                        [IDLValue::Blob(b1), IDLValue::Blob(b2)] => {
                            let mut res = Vec::from(b1.as_slice());
                            res.extend_from_slice(b2);
                            IDLValue::Blob(res)
                        }
                        [IDLValue::Text(s1), IDLValue::Text(s2)] => {
                            IDLValue::Text(String::from(s1) + s2)
                        }
                        [IDLValue::Record(f1), IDLValue::Record(f2)] => {
                            let mut fs = Vec::from(f1.as_slice());
                            fs.extend_from_slice(f2);
                            fs.sort_unstable_by_key(|IDLField { id, .. }| id.get_id());
                            check_unique(fs.iter().map(|f| &f.id))?;
                            IDLValue::Record(fs)
                        }
                        _ => return Err(anyhow!("concat expects two vec, record or text")),
                    },
                    "eq" | "neq" => match args.as_slice() {
                        [v1, v2] => {
                            if v1.value_ty() != v2.value_ty() {
                                return Err(anyhow!(
                                    "{} expects two values of the same type",
                                    func
                                ));
                            }
                            IDLValue::Bool(match func.as_str() {
                                "eq" => v1 == v2,
                                "neq" => v1 != v2,
                                _ => unreachable!(),
                            })
                        }
                        _ => return Err(anyhow!("{func} expects two values")),
                    },
                    "and" | "or" => match args.as_slice() {
                        [IDLValue::Bool(v1), IDLValue::Bool(v2)] => {
                            IDLValue::Bool(match func.as_str() {
                                "and" => *v1 && *v2,
                                "or" => *v1 || *v2,
                                _ => unreachable!(),
                            })
                        }
                        _ => return Err(anyhow!("{func} expects bool values")),
                    },
                    "not" => match args.as_slice() {
                        [IDLValue::Bool(v)] => IDLValue::Bool(!v),
                        _ => return Err(anyhow!("not expects a bool value")),
                    },
                    "lt" | "lte" | "gt" | "gte" | "add" | "sub" | "mul" | "div" => match args
                        .as_slice()
                    {
                        [IDLValue::Float32(_) | IDLValue::Float64(_), _]
                        | [_, IDLValue::Float32(_) | IDLValue::Float64(_)] => {
                            let IDLValue::Float64(v1) =
                                cast_type(args[0].clone(), &TypeInner::Float64.into())?
                            else {
                                panic!()
                            };
                            let IDLValue::Float64(v2) =
                                cast_type(args[1].clone(), &TypeInner::Float64.into())?
                            else {
                                panic!()
                            };
                            match func.as_str() {
                                "add" => IDLValue::Float64(v1 + v2),
                                "sub" => IDLValue::Float64(v1 - v2),
                                "mul" => IDLValue::Float64(v1 * v2),
                                "div" => IDLValue::Float64(v1 / v2),
                                "lt" => IDLValue::Bool(v1 < v2),
                                "lte" => IDLValue::Bool(v1 <= v2),
                                "gt" => IDLValue::Bool(v1 > v2),
                                "gte" => IDLValue::Bool(v1 >= v2),
                                _ => unreachable!(),
                            }
                        }
                        [v1, v2] => {
                            let IDLValue::Int(v1) = cast_type(v1.clone(), &TypeInner::Int.into())?
                            else {
                                panic!()
                            };
                            let IDLValue::Int(v2) = cast_type(v2.clone(), &TypeInner::Int.into())?
                            else {
                                panic!()
                            };
                            match func.as_str() {
                                "add" => IDLValue::Number((v1 + v2).to_string()),
                                "sub" => IDLValue::Number((v1 - v2).to_string()),
                                "mul" => IDLValue::Number((v1 * v2).to_string()),
                                "div" => IDLValue::Number((v1 / v2).to_string()),
                                "lt" => IDLValue::Bool(v1 < v2),
                                "lte" => IDLValue::Bool(v1 <= v2),
                                "gt" => IDLValue::Bool(v1 > v2),
                                "gte" => IDLValue::Bool(v1 >= v2),
                                _ => unreachable!(),
                            }
                        }
                        _ => return Err(anyhow!("{func} expects two numbers")),
                    },
                    func => apply_func(helper, func, args)?,
                }
            }
            Exp::Decode { method, blob } => {
                let blob = blob.eval(helper)?;
                if *blob.value_ty() != TypeInner::Vec(TypeInner::Nat8.into()) {
                    return Err(anyhow!("not a blob"));
                }
                let bytes: Vec<u8> = match blob {
                    IDLValue::Blob(b) => b,
                    IDLValue::Vec(vs) => vs
                        .into_iter()
                        .map(|v| match v {
                            IDLValue::Nat8(u) => u,
                            _ => unreachable!(),
                        })
                        .collect(),
                    _ => unreachable!(),
                };
                let args = match method {
                    Some(method) => {
                        let info = method.get_info(helper, false)?;
                        if let Some((env, func)) = info.signature {
                            IDLArgs::from_bytes_with_types(&bytes, &env, &func.rets)?
                        } else {
                            IDLArgs::from_bytes(&bytes)?
                        }
                    }
                    None => IDLArgs::from_bytes(&bytes)?,
                };
                args_to_value(args)
            }
            Exp::ParCall { calls } => {
                let mut futures = Vec::with_capacity(calls.len());
                for call in calls {
                    let mut args = Vec::with_capacity(call.args.len());
                    for arg in call.args.into_iter() {
                        args.push(arg.eval(helper)?);
                    }
                    let args = IDLArgs { args };
                    let info = call.method.get_info(helper, false)?;
                    let bytes = if let Some((env, func)) = &info.signature {
                        args.to_bytes_with_types(env, &func.args)?
                    } else {
                        args.to_bytes()?
                    };
                    let method = &call.method.method;
                    let effective_id = get_effective_canister_id(info.canister_id, method, &bytes)?
                        .unwrap_or(helper.default_effective_canister_id);
                    let mut builder = helper.agent.update(&info.canister_id, method);
                    builder = builder
                        .with_arg(bytes)
                        .with_effective_canister_id(effective_id);
                    let call_future = async move {
                        let res = builder.call_and_wait().await?;
                        if let Some((env, func)) = &info.signature {
                            Ok(IDLArgs::from_bytes_with_types(&res, env, &func.rets)?)
                        } else {
                            Ok(IDLArgs::from_bytes(&res)?)
                        }
                    };
                    futures.push(call_future);
                }
                let res = parallel_calls(futures)?;
                let res = IDLArgs {
                    args: res.into_iter().map(args_to_value).collect(),
                };
                args_to_value(res)
            }
            Exp::Call { method, args, mode } => {
                let args = if let Some(args) = args {
                    let mut res = Vec::with_capacity(args.len());
                    for arg in args.into_iter() {
                        res.push(arg.eval(helper)?);
                    }
                    Some(IDLArgs { args: res })
                } else {
                    None
                };
                let opt_info = if let Some(method) = &method {
                    let is_encode = matches!(mode, CallMode::Encode);
                    Some(method.get_info(helper, is_encode)?)
                } else {
                    None
                };
                let bytes = if let Some(MethodInfo {
                    signature: Some((env, func)),
                    ..
                }) = &opt_info
                {
                    let args = if let Some(args) = args {
                        args
                    } else {
                        use candid_parser::assist::{input_args, Context};
                        let mut ctx = Context::new(env.clone());
                        let principals = helper.env.dump_principals();
                        let mut completion = BTreeMap::new();
                        completion.insert("principal".to_string(), principals);
                        ctx.set_completion(completion);
                        let args = input_args(&ctx, &func.args)?;
                        // Ideally, we should store the args in helper and call editor.readline_with_initial to display
                        // the full command in the editor. The tricky part is to know where to insert the args in text.
                        eprintln!("Generated arguments: {}", args);
                        eprintln!("Do you want to send this message? [y/N]");
                        let mut input = String::new();
                        std::io::stdin().read_line(&mut input)?;
                        if !["y", "yes"].contains(&input.to_lowercase().trim()) {
                            return Err(anyhow!("Abort"));
                        }
                        args
                    };
                    args.to_bytes_with_types(env, &func.args)?
                } else {
                    if args.is_none() {
                        return Err(anyhow!("cannot get method type, please provide arguments"));
                    }
                    args.unwrap().to_bytes()?
                };
                match mode {
                    CallMode::Encode => IDLValue::Blob(bytes),
                    CallMode::Call => {
                        use crate::profiling::{get_cycles, ok_to_profile};
                        let method = method.unwrap(); // okay to unwrap from parser
                        let info = opt_info.unwrap();
                        let ok_to_profile = ok_to_profile(helper, &info);
                        let before_cost = if ok_to_profile {
                            get_cycles(&helper.agent, &info.canister_id)?
                        } else {
                            0
                        };
                        let res = call(
                            helper,
                            &info.canister_id,
                            &method.method,
                            &bytes,
                            &info.signature,
                            &helper.offline,
                        )?;
                        if ok_to_profile {
                            let cost = get_cycles(&helper.agent, &info.canister_id)? - before_cost;
                            println!("Cost: {cost} Wasm instructions");
                            let cost = IDLValue::Record(vec![IDLField {
                                id: Label::Named("__cost".to_string()),
                                val: IDLValue::Int64(cost),
                            }]);
                            let res = IDLArgs::new(&[args_to_value(res), cost]);
                            args_to_value(res)
                        } else {
                            args_to_value(res)
                        }
                    }
                    CallMode::Proxy(id) => {
                        let method = method.unwrap();
                        let canister_id = str_to_principal(&method.canister, helper)?;
                        let proxy_id = str_to_principal(&id, helper)?;
                        let mut env = MyHelper::new(
                            helper.agent.clone(),
                            helper.agent_url.clone(),
                            helper.offline.clone(),
                            helper.verbose,
                        );
                        env.canister_map.borrow_mut().0.insert(
                            proxy_id,
                            helper
                                .canister_map
                                .borrow()
                                .0
                                .get(&proxy_id)
                                .ok_or_else(|| {
                                    anyhow!("{} canister interface not found", proxy_id)
                                })?
                                .clone(),
                        );
                        env.env.0.insert("_msg".to_string(), IDLValue::Blob(bytes));
                        let code = format!(
                            r#"
let _ = call "{id}".wallet_call(
  record {{
    args = _msg;
    cycles = 0;
    method_name = "{method}";
    canister = principal "{canister}";
  }}
);
let _ = decode as "{canister}".{method} _.Ok.return;
"#,
                            id = proxy_id,
                            canister = canister_id,
                            method = method.method
                        );
                        let cmds = pretty_parse::<crate::command::Commands>("forward_call", &code)?;
                        for (cmd, _) in cmds.0.into_iter() {
                            cmd.run(&mut env)?;
                        }
                        env.env.0.get("_").unwrap().clone()
                    }
                }
            }
            Exp::Bool(b) => IDLValue::Bool(b),
            Exp::Null => IDLValue::Null,
            Exp::Text(s) => IDLValue::Text(s),
            Exp::Number(n) => IDLValue::Number(n),
            Exp::Float64(f) => IDLValue::Float64(f),
            Exp::Principal(id) => IDLValue::Principal(id),
            Exp::Service(id) => IDLValue::Service(id),
            Exp::Func(id, meth) => IDLValue::Func(id, meth),
            Exp::Opt(v) => IDLValue::Opt(Box::new((*v).eval(helper)?)),
            Exp::Blob(b) => IDLValue::Blob(b),
            Exp::Vec(vs) => {
                let mut vec = Vec::with_capacity(vs.len());
                for v in vs.into_iter() {
                    vec.push(v.eval(helper)?);
                }
                IDLValue::Vec(vec)
            }
            Exp::Record(fs) => {
                let mut res = Vec::with_capacity(fs.len());
                for Field { id, val } in fs.into_iter() {
                    res.push(IDLField {
                        id,
                        val: val.eval(helper)?,
                    });
                }
                IDLValue::Record(res)
            }
            Exp::Variant(f, idx) => {
                let f = IDLField {
                    id: f.id,
                    val: f.val.eval(helper)?,
                };
                IDLValue::Variant(VariantValue(Box::new(f), idx))
            }
        })
    }
}

impl std::str::FromStr for Exp {
    type Err = ParserError;
    fn from_str(str: &str) -> Result<Self, Self::Err> {
        let lexer = Tokenizer::new(str);
        super::grammar::ExpParser::new().parse(lexer)
    }
}

#[derive(Debug)]
pub struct MethodInfo {
    pub canister_id: Principal,
    pub signature: Option<(TypeEnv, Function)>,
    pub profiling: Option<BTreeMap<u16, String>>,
}
impl Method {
    pub fn get_info(&self, helper: &MyHelper, is_encode: bool) -> Result<MethodInfo> {
        if is_encode && self.method == "__init_args" {
            if let Some(IDLValue::Blob(bytes)) = helper.env.0.get(&self.canister) {
                use ic_wasm::{metadata::get_metadata, utils::parse_wasm};
                let m = parse_wasm(bytes, false)?;
                let args = get_metadata(&m, "candid:args");
                let candid = get_metadata(&m, "candid:service");
                let canister_id = Principal::anonymous();
                match args {
                    None => {
                        eprintln!("Warning: no candid:args metadata in the Wasm module, use types inferred from textual value.");
                        return Ok(MethodInfo {
                            canister_id,
                            signature: None,
                            profiling: None,
                        });
                    }
                    Some(args) => {
                        let candid = candid
                            .as_ref()
                            .map(|x| std::str::from_utf8(x).unwrap())
                            .unwrap_or("service : {}");
                        let (env, ty) = candid_parser::utils::merge_init_args(
                            candid,
                            std::str::from_utf8(&args)?,
                        )?;
                        let init_args = find_init_args(&env, &ty).expect("invalid init arg types");
                        let signature = Some((
                            env,
                            Function {
                                args: init_args,
                                rets: Vec::new(),
                                modes: Vec::new(),
                            },
                        ));
                        return Ok(MethodInfo {
                            canister_id,
                            signature,
                            profiling: None,
                        });
                    }
                }
            }
        }
        let canister_id = str_to_principal(&self.canister, helper)?;
        let agent = &helper.agent;
        let mut map = helper.canister_map.borrow_mut();
        Ok(match map.get(agent, &canister_id) {
            Err(_) => MethodInfo {
                canister_id,
                signature: None,
                profiling: None,
            },
            Ok(info) => {
                let signature = if self.method == "__init_args" {
                    eprintln!(
                        "Warning: no init args in did file, use types inferred from textual value."
                    );
                    info.init.clone().map(|init| {
                        (
                            info.env.clone(),
                            Function {
                                args: init,
                                rets: Vec::new(),
                                modes: Vec::new(),
                            },
                        )
                    })
                } else {
                    info.methods
                        .get(&self.method)
                        .or_else(|| {
                            if !self.method.starts_with("__") {
                                eprintln!(
                                    "Warning: cannot get type for {}.{}, use types infered from textual value",
                                    self.canister, self.method
                                );
                            }
                            None
                        })
                        .map(|ty| (info.env.clone(), ty.clone()))
                };
                MethodInfo {
                    canister_id,
                    signature,
                    profiling: info.profiling.clone(),
                }
            }
        })
    }
}

pub fn apply_func(helper: &MyHelper, func: &str, args: Vec<IDLValue>) -> Result<IDLValue> {
    match helper.func_env.0.get(func) {
        None => Err(anyhow!("Unknown function {}", func)),
        Some((formal_args, body)) => {
            if formal_args.len() != args.len() {
                return Err(anyhow!(
                    "{} expects {} arguments, but {} is provided",
                    func,
                    formal_args.len(),
                    args.len()
                ));
            }
            let mut helper = helper.spawn();
            for (id, v) in formal_args.iter().zip(args.into_iter()) {
                helper.env.0.insert(id.to_string(), v);
            }
            for cmd in body.iter() {
                cmd.clone().run(&mut helper)?;
            }
            let res = helper.env.0.get("_").unwrap_or(&IDLValue::Null).clone();
            Ok(res)
        }
    }
}
#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn parallel_calls(
    futures: Vec<impl std::future::Future<Output = anyhow::Result<IDLArgs>>>,
) -> anyhow::Result<Vec<IDLArgs>> {
    let res = try_join_all(futures).await?;
    Ok(res)
}
#[tokio::main]
async fn call(
    helper: &MyHelper,
    canister_id: &Principal,
    method: &str,
    args: &[u8],
    opt_func: &Option<(TypeEnv, Function)>,
    offline: &Option<OfflineOutput>,
) -> anyhow::Result<IDLArgs> {
    use crate::offline::*;
    let agent = &helper.agent;
    let effective_id = get_effective_canister_id(*canister_id, method, args)?
        .unwrap_or(helper.default_effective_canister_id);
    let is_query = opt_func
        .as_ref()
        .map(|(_, f)| f.is_query())
        .unwrap_or(false);
    let bytes = if is_query {
        let mut builder = agent.query(canister_id, method);
        builder = builder
            .with_arg(args)
            .with_effective_canister_id(effective_id);
        if let Some(offline) = offline {
            let mut msgs = helper.messages.borrow_mut();
            let signed = builder.sign()?;
            let message = IngressWithStatus {
                ingress: Ingress {
                    call_type: "query".to_owned(),
                    request_id: None,
                    content: hex::encode(signed.signed_query),
                },
                request_status: None,
            };
            msgs.push(message.clone());
            output_message(serde_json::to_string(&message)?, offline)?;
            return Ok(IDLArgs::new(&[]));
        } else {
            builder.call().await?
        }
    } else {
        let mut builder = agent.update(canister_id, method);
        builder = builder
            .with_arg(args)
            .with_effective_canister_id(effective_id);
        if let Some(offline) = offline {
            let mut msgs = helper.messages.borrow_mut();
            let signed = builder.sign()?;
            let status = agent.sign_request_status(effective_id, signed.request_id)?;
            let message = IngressWithStatus {
                ingress: Ingress {
                    call_type: "update".to_owned(),
                    request_id: Some(hex::encode(signed.request_id.as_slice())),
                    content: hex::encode(signed.signed_update),
                },
                request_status: Some(RequestStatus {
                    canister_id: status.effective_canister_id,
                    request_id: hex::encode(status.request_id.as_slice()),
                    content: hex::encode(status.signed_request_status),
                }),
            };
            msgs.push(message.clone());
            output_message(serde_json::to_string(&message)?, offline)?;
            return Ok(IDLArgs::new(&[]));
        } else {
            builder.call_and_wait().await?
        }
    };
    let res = if let Some((env, func)) = opt_func {
        IDLArgs::from_bytes_with_types(&bytes, env, &func.rets)?
    } else {
        IDLArgs::from_bytes(&bytes)?
    };
    Ok(res)
}
