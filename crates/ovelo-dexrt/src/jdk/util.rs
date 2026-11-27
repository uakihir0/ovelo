//! java.util leftovers: Timer, Observable/EventObject, Currency,
//! service loader, stream stubs.

use super::*;
use crate::value::{HostState, Obj, V};

fn stream_items(vm: &Vm, v: V) -> Vec<V> {
    match v {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Host {
                state: HostState::List(items),
                ..
            }) => items.clone(),
            _ => vec![],
        },
        _ => vec![],
    }
}

fn stream_obj(vm: &mut Vm, items: Vec<V>) -> V {
    host(vm, "Ljava/util/stream/Stream;", HostState::List(items))
}

fn i64num(v: V) -> i64 {
    match v {
        V::I32(i) => i as i64,
        V::I64(i) => i,
        V::F32(f) => f as i64,
        V::F64(f) => f as i64,
        _ => 0,
    }
}

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    _sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    Ok(
        if class == "Ljava/util/Timer;" || class == "Ljava/util/TimerTask;" {
            match name {
                "schedule" | "scheduleAtFixedRate" => {
                    // Run the task inline once — deterministic single-threaded model.
                    if let Some(V::Ref(r)) = args.get(1).copied() {
                        let _ = vm.call_virtual(r, "run", &[]);
                    }
                    Some(None)
                }
                "cancel" | "purge" => Some(match name {
                    "purge" => Some(V::I32(0)),
                    _ => None,
                }),
                "scheduledExecutionTime" => Some(Some(V::I64(now_ms()))),
                _ => None,
            }
        } else if class == "Ljava/util/Observable;" {
            match name {
                "addObserver" | "deleteObserver" | "deleteObservers" | "setChanged"
                | "clearChanged" | "notifyObservers" => Some(None),
                "hasChanged" | "countObservers" => Some(Some(V::I32(0))),
                _ => None,
            }
        } else if class == "Ljava/util/EventObject;" {
            match name {
                "getSource" => Some(Some(match args[0] {
                    V::Ref(h) => slot_of(vm, h).unwrap_or(V::Ref(0)),
                    v => v,
                })),
                _ => None,
            }
        } else if class == "Ljava/util/ResourceBundle;"
            || class == "Ljava/util/PropertyResourceBundle;"
            || class == "Ljava/util/ListResourceBundle;"
        {
            match name {
                "getBundle" => Some(Some(host(
                    vm,
                    "Ljava/util/ResourceBundle;",
                    HostState::Empty,
                ))),
                // No .properties loading yet — answer the key itself so
                // localized UI text degrades to identifiers instead of a
                // MissingResourceException aborting the call path.
                "getString" => {
                    let k = args.iter().find_map(|a| sv(vm, *a)).unwrap_or_default();
                    Some(Some(jstr(vm, &k)))
                }
                "getStringArray" => Some(Some(arr(vm, "Ljava/lang/String;", vec![]))),
                "getKeys" | "keySet" | "containsKey" => Some(match name {
                    "containsKey" => Some(V::I32(0)),
                    "keySet" => Some(host(vm, "Ljava/util/Set;", HostState::List(vec![]))),
                    _ => Some(host(vm, "Ljava/util/Enumeration;", HostState::Empty)),
                }),
                "getObject" => Some(Some(V::Ref(0))),
                _ => None,
            }
        } else if class == "Ljava/util/Currency;" {
            match name {
                "getInstance" => Some(Some(host(vm, class, HostState::StrBuf("USD".into())))),
                "getCurrencyCode" | "getSymbol" | "getDisplayName" => Some(Some(jstr(vm, "USD"))),
                "getDefaultFractionDigits" => Some(Some(V::I32(2))),
                "getNumericCode" | "getNumericCodeAsString" => Some(Some(V::I32(840))),
                "getAvailableCurrencies" => {
                    Some(Some(host(vm, "Ljava/util/Set;", HostState::List(vec![]))))
                }
                _ => None,
            }
        } else if class == "Ljava/util/ServiceLoader;"
            || class.starts_with("Ljava/util/stream/")
            || class.starts_with("Ljava/util/concurrent/locks/")
            || class.starts_with("Ljava/util/concurrent/ForkJoin")
            || class == "Ljava/util/concurrent/Future;"
            || class == "Ljava/util/concurrent/CompletableFuture;"
        {
            match name {
                "iterator" => {
                    let items = stream_items(vm, args.first().copied().unwrap_or(V::Ref(0)));
                    Some(Some(host(
                        vm,
                        "Ljava/util/Iterator;",
                        HostState::Iter(std::rc::Rc::new(items), 0),
                    )))
                }
                "load" | "loadInstalled" | "stream" => {
                    Some(Some(host(vm, class, HostState::Empty)))
                }
                "collect" | "toList" | "toArray" | "count" | "forEach" | "forEachOrdered"
                | "filter" | "map" | "flatMap" | "mapToInt" | "mapToLong" | "mapToDouble"
                | "reduce" | "findFirst" | "findAny" | "anyMatch" | "allMatch" | "noneMatch"
                | "min" | "max" | "sorted" | "distinct" | "limit" | "skip" | "peek" | "onClose"
                | "close" | "isParallel" | "sequential" | "parallel" | "unordered"
                | "iterator0" | "spliterator" => {
                    let this = args.first().copied().unwrap_or(V::Ref(0));
                    let items = stream_items(vm, this);
                    match name {
                        "forEach" | "forEachOrdered" | "peek" => {
                            if let Some(V::Ref(consumer)) = args.get(1).copied() {
                                for item in items.iter().copied() {
                                    vm.call_virtual_rt(consumer, "accept", &[item])?;
                                }
                            }
                            if name == "peek" {
                                Some(Some(stream_obj(vm, items)))
                            } else {
                                Some(None)
                            }
                        }
                        "filter" => {
                            let mut out = Vec::new();
                            if let Some(V::Ref(pred)) = args.get(1).copied() {
                                for item in items {
                                    let keep = vm
                                        .call_virtual_rt(pred, "test", &[item])?
                                        .map(|v| i32v(v) != 0)
                                        .unwrap_or(false);
                                    if keep {
                                        out.push(item);
                                    }
                                }
                            }
                            Some(Some(stream_obj(vm, out)))
                        }
                        "map" | "mapToInt" | "mapToLong" | "mapToDouble" | "flatMap" => {
                            let mut out = Vec::new();
                            if let Some(V::Ref(f)) = args.get(1).copied() {
                                for item in items {
                                    if let Some(mapped) = vm.call_virtual_rt(f, "apply", &[item])? {
                                        if name == "flatMap" {
                                            out.extend(stream_items(vm, mapped));
                                        } else {
                                            out.push(mapped);
                                        }
                                    }
                                }
                            }
                            Some(Some(stream_obj(vm, out)))
                        }
                        "limit" => {
                            let n =
                                i64num(args.get(1).copied().unwrap_or(V::I64(0))).max(0) as usize;
                            Some(Some(stream_obj(vm, items.into_iter().take(n).collect())))
                        }
                        "skip" => {
                            let n =
                                i64num(args.get(1).copied().unwrap_or(V::I64(0))).max(0) as usize;
                            Some(Some(stream_obj(vm, items.into_iter().skip(n).collect())))
                        }
                        "count" => Some(Some(V::I64(items.len() as i64))),
                        "toList" => {
                            Some(Some(host(vm, "Ljava/util/List;", HostState::List(items))))
                        }
                        "toArray" => Some(Some(arr(vm, "Ljava/lang/Object;", items))),
                        "findFirst" | "findAny" => {
                            let v = items.first().copied().unwrap_or(V::Ref(0));
                            Some(Some(host(vm, "Ljava/util/Optional;", HostState::Slot(v))))
                        }
                        "anyMatch" | "allMatch" | "noneMatch" => {
                            let mut yes = 0usize;
                            let mut total = 0usize;
                            if let Some(V::Ref(pred)) = args.get(1).copied() {
                                for item in items {
                                    total += 1;
                                    if vm
                                        .call_virtual_rt(pred, "test", &[item])?
                                        .map(|v| i32v(v) != 0)
                                        .unwrap_or(false)
                                    {
                                        yes += 1;
                                    }
                                }
                            }
                            let r = if name == "anyMatch" {
                                yes != 0
                            } else if name == "allMatch" {
                                yes == total
                            } else {
                                yes == 0
                            };
                            Some(Some(V::I32(r as i32)))
                        }
                        "iterator0" => Some(Some(host(
                            vm,
                            "Ljava/util/Iterator;",
                            HostState::Iter(std::rc::Rc::new(items), 0),
                        ))),
                        "isParallel" => Some(Some(V::I32(0))),
                        "close" | "onClose" | "sequential" | "parallel" | "unordered"
                        | "sorted" | "distinct" => Some(Some(args[0])),
                        "min" | "max" | "reduce" | "collect" | "spliterator" => {
                            if matches!(name, "min" | "max") {
                                let v = items.first().copied().unwrap_or(V::Ref(0));
                                Some(Some(host(vm, "Ljava/util/Optional;", HostState::Slot(v))))
                            } else if name == "reduce" && items.len() == 1 {
                                Some(Some(items[0]))
                            } else {
                                Some(Some(args[0]))
                            }
                        }
                        _ => Some(Some(args[0])),
                    }
                }
                "of" | "empty" | "concat" | "builder" | "generate" | "iterate" | "range"
                | "rangeClosed" => Some(Some(host(
                    vm,
                    "Ljava/util/stream/Stream;",
                    HostState::Empty,
                ))),
                "supplyAsync" | "runAsync" | "completedFuture" | "allOf" | "anyOf"
                | "failedFuture" => Some(Some(host(
                    vm,
                    "Ljava/util/concurrent/CompletableFuture;",
                    HostState::Empty,
                ))),
                "get"
                | "join"
                | "getNow"
                | "complete"
                | "thenApply"
                | "thenAccept"
                | "thenRun"
                | "thenCompose"
                | "exceptionally"
                | "handle"
                | "whenComplete"
                | "completeExceptionally"
                | "isDone"
                | "isCompletedExceptionally"
                | "isCancelled"
                | "cancel"
                | "obtrudeValue"
                | "newIncompleteFuture"
                | "delayedExecutor" => {
                    if matches!(
                        name,
                        "isDone"
                            | "isCompletedExceptionally"
                            | "isCancelled"
                            | "complete"
                            | "completeExceptionally"
                            | "cancel"
                    ) {
                        Some(Some(V::I32(matches!(name, "isDone") as i32)))
                    } else if name == "complete" {
                        Some(Some(V::I32(1)))
                    } else if matches!(name, "get" | "join" | "getNow") {
                        if let Some(v) = args.first().and_then(|v| match v {
                            V::Ref(h) => slot_of(vm, *h),
                            _ => None,
                        }) {
                            Some(Some(v))
                        } else {
                            Some(Some(args[0]))
                        }
                    } else {
                        Some(Some(args[0]))
                    }
                }
                "lock"
                | "unlock"
                | "tryLock"
                | "lockInterruptibly"
                | "newCondition"
                | "readLock"
                | "writeLock"
                | "readLockCount"
                | "getWriteHoldCount"
                | "getReadHoldCount"
                | "isWriteLocked"
                | "isWriteLockedByCurrentThread"
                | "signal"
                | "signalAll"
                | "await"
                | "awaitNanos"
                | "awaitUntil" => {
                    if matches!(
                        name,
                        "tryLock" | "isWriteLocked" | "isWriteLockedByCurrentThread" | "await"
                    ) {
                        Some(Some(V::I32(1)))
                    } else if matches!(name, "newCondition") {
                        Some(Some(host(
                            vm,
                            "Ljava/util/concurrent/locks/Condition;",
                            HostState::Empty,
                        )))
                    } else if matches!(name, "readLock" | "writeLock") {
                        // ReentrantReadWriteLock.readLock()/writeLock() return
                        // the Lock views — a no-op Lock host object keeps
                        // `lock.lock(); …; lock.unlock()` chains flowing.
                        Some(Some(host(
                            vm,
                            "Ljava/util/concurrent/locks/Lock;",
                            HostState::Empty,
                        )))
                    } else {
                        Some(None)
                    }
                }
                _ => None,
            }
        } else {
            return Ok(None);
        },
    )
}
