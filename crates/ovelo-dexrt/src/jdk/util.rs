//! java.util leftovers: Timer, Observable/EventObject, Currency,
//! service loader, stream stubs.

use super::*;
use crate::value::{HostState, V};

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
                "iterator" => Some(Some(host(
                    vm,
                    "Ljava/util/Iterator;",
                    HostState::Iter(std::rc::Rc::new(vec![]), 0),
                ))),
                "load" | "loadInstalled" | "stream" => {
                    Some(Some(host(vm, class, HostState::Empty)))
                }
                "collect" | "toList" | "toArray" | "count" | "forEach" | "forEachOrdered"
                | "filter" | "map" | "flatMap" | "mapToInt" | "mapToLong" | "mapToDouble"
                | "reduce" | "findFirst" | "findAny" | "anyMatch" | "allMatch" | "noneMatch"
                | "min" | "max" | "sorted" | "distinct" | "limit" | "skip" | "peek" | "onClose"
                | "close" | "isParallel" | "sequential" | "parallel" | "unordered"
                | "iterator0" | "spliterator" => {
                    if matches!(
                        name,
                        "count" | "anyMatch" | "allMatch" | "noneMatch" | "isParallel"
                    ) {
                        Some(Some(V::I32(0)))
                    } else if matches!(name, "toArray" | "toList") {
                        Some(Some(host(vm, "Ljava/util/List;", HostState::List(vec![]))))
                    } else if matches!(name, "findFirst" | "findAny" | "min" | "max") {
                        Some(Some(host(
                            vm,
                            "Ljava/util/Optional;",
                            HostState::Slot(V::Ref(0)),
                        )))
                    } else {
                        Some(Some(args[0]))
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
