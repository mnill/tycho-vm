use std::rc::Rc;

use everscale_types::cell::{
    self, Cell, CellBuilder, CellContext, CellTreeStats, HashBytes, Load, LoadMode, StorageStat,
};
use everscale_types::dict;
use everscale_types::models::{
    ChangeLibraryMode, CurrencyCollection, ExtAddr, ExtraCurrencyCollection, Lazy, LibRef,
    MessageLayout, MsgForwardPrices, OutAction, RelaxedMessage, RelaxedMsgInfo,
    ReserveCurrencyFlags, SendMsgFlags, SizeLimitsConfig,
};
use everscale_types::num::{SplitDepth, Tokens};
use everscale_vm::util::load_uint_leq;
use everscale_vm_proc::vm_module;
use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;

use crate::cont::ControlRegs;
use crate::error::VmResult;
use crate::stack::{Stack, Tuple, TupleExt};
use crate::state::{GasConsumer, VmState, VmVersion};
use crate::util::{MsgForwardPricesExt, OwnedCellSlice};

pub struct MessageOps;

#[vm_module]
impl MessageOps {
    #[instr(code = "fb00", fmt = "SENDRAWMSG")]
    fn exec_send_message_raw(st: &mut VmState) -> VmResult<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let mode = ok!(stack.pop_smallint_range(0, 255)) as u8;
        let cell = ok!(stack.pop_cell());

        add_action(&mut st.cr, &mut st.gas, OutAction::SendMsg {
            mode: SendMsgFlags::from_bits_retain(mode),
            out_msg: Lazy::from_raw(Rc::unwrap_or_clone(cell)),
        })
    }

    #[instr(code = "fb02", fmt = "RAWRESERVE", args(x = false))]
    #[instr(code = "fb03", fmt = "RAWRESERVEX", args(x = true))]
    fn exec_reserve_raw(st: &mut VmState, x: bool) -> VmResult<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let mode = ok!(stack.pop_smallint_range(
            0,
            if st.version.is_ton(4..) {
                0b11111
            } else {
                0b1111
            }
        ));
        let other = if x { ok!(stack.pop_cell_opt()) } else { None };
        let tokens = ok!(stack.pop_int().and_then(|int| bigint_to_tokens(&int)));

        add_action(&mut st.cr, &mut st.gas, OutAction::ReserveCurrency {
            mode: ReserveCurrencyFlags::from_bits_retain(mode as u8),
            value: CurrencyCollection {
                tokens,
                other: ExtraCurrencyCollection::from_raw(other.map(Rc::unwrap_or_clone)),
            },
        })
    }

    #[instr(code = "fb04", fmt = "SETCODE")]
    fn exec_set_code(st: &mut VmState) -> VmResult<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let code = ok!(stack.pop_cell());

        add_action(&mut st.cr, &mut st.gas, OutAction::SetCode {
            new_code: Rc::unwrap_or_clone(code),
        })
    }

    #[instr(code = "fb06", fmt = "SETLIBCODE")]
    fn exec_set_lib_code(st: &mut VmState) -> VmResult<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let mode = ok!(pop_change_library_mode(st.version, stack));
        let code = ok!(stack.pop_cell());

        add_action(&mut st.cr, &mut st.gas, OutAction::ChangeLibrary {
            mode,
            lib: LibRef::Cell(Rc::unwrap_or_clone(code)),
        })
    }

    #[instr(code = "fb07", fmt = "CHANGELIB")]
    fn exec_change_lib(st: &mut VmState) -> VmResult<i32> {
        let stack = Rc::make_mut(&mut st.stack);
        let mode = ok!(pop_change_library_mode(st.version, stack));
        let hash = {
            let int = ok!(stack.pop_int());
            vm_ensure!(int.sign() != Sign::Minus, IntegerOutOfRange {
                min: 0,
                max: isize::MAX,
                actual: int.to_string(),
            });

            let mut bytes = int.magnitude().to_bytes_le();
            bytes.truncate(32);
            bytes.reverse();

            let mut res = HashBytes::ZERO;
            res.0[32 - bytes.len()..32].copy_from_slice(&bytes);
            res
        };

        add_action(&mut st.cr, &mut st.gas, OutAction::ChangeLibrary {
            mode,
            lib: LibRef::Hash(hash),
        })
    }

    #[instr(code = "fb08", fmt = "SENDMSG")]
    fn exec_send_message(st: &mut VmState) -> VmResult<i32> {
        ok!(st.version.require_ton(4..));

        // Get args from the stack.
        let stack = Rc::make_mut(&mut st.stack);
        let (mode, send) = ok!(pop_send_msg_mode_ext(stack));
        let raw_msg_cell = ok!(stack.pop_cell());
        let msg_cell = st
            .gas
            .context()
            .load_cell(Cell::clone(&raw_msg_cell), LoadMode::Full)?;
        let msg = msg_cell.parse::<RelaxedMessage<'_>>()?;

        // Parse c7 tuple.
        let t1 = ok!(st.cr.get_c7_params());
        let t2 = if st.version.is_ton(6..) {
            Some(ok!(t1.try_get_tuple_range(
                VmState::PARSED_CONFIG_GLOBAL_IDX,
                0..=255
            )))
        } else {
            None
        };
        let my_addr = ok!(t1.try_get_ref::<OwnedCellSlice>(VmState::MYADDR_GLOBAL_IDX));
        let my_workchain = ok!(parse_addr_workchain(my_addr));

        // Prefetch msg info.
        let mut is_masterchain = my_workchain == -1;
        let mut ihr_disabled = true;
        let mut value = Tokens::ZERO;
        let mut has_extra_currencies = false;
        let mut user_fwd_fee = Tokens::ZERO;
        let mut user_ihr_fee = Tokens::ZERO;
        if let RelaxedMsgInfo::Int(info) = &msg.info {
            is_masterchain |= info.dst.is_masterchain();
            ihr_disabled = info.ihr_disabled;
            value = info.value.tokens;
            has_extra_currencies = !info.value.other.is_empty();
            user_fwd_fee = info.fwd_fee;
            user_ihr_fee = info.ihr_fee;
        }

        // Get message forwarding prices.
        let prices = match t2 {
            Some(t2) => {
                let cs = ok!(t2.try_get_ref::<OwnedCellSlice>(if is_masterchain { 4 } else { 5 }));
                MsgForwardPrices::load_from(&mut cs.apply()?)?
            }
            None => {
                let config_root = ok!(t1.try_get_ref::<Cell>(VmState::CONFIG_GLOBAL_IDX));

                let mut b = CellBuilder::new();
                b.store_u32(if is_masterchain { 24 } else { 25 }).unwrap();
                let key = b.as_data_slice();

                let Some(mut value) = dict::dict_get(Some(config_root), 32, key, st.gas.context())?
                else {
                    vm_bail!(Unknown("invalid prices config".to_owned()));
                };

                let param = value.load_reference()?;
                st.gas
                    .context()
                    .load_dyn_cell(param, LoadMode::Full)?
                    .parse::<MsgForwardPrices>()?
            }
        };

        // Compute storage stat for message child cells.
        let max_cells = match t2 {
            Some(t2) => {
                let cs = ok!(t2.try_get_ref::<OwnedCellSlice>(6));
                let limits = SizeLimitsConfig::load_from(&mut cs.apply()?)?;
                limits.max_msg_cells
            }
            None => 1 << 13,
        };
        let mut stats = {
            let mut st = StorageStat::with_limit(max_cells as _);
            let mut cs = msg_cell.as_slice()?;
            cs.skip_first(cs.size_bits(), 0).ok();
            st.add_slice(&cs);
            st.stats()
        };

        // Adjust outgoing message value and extra currencies.
        if matches!(&msg.info, RelaxedMsgInfo::Int(_)) {
            if mode.contains(SendMsgFlags::ALL_BALANCE) {
                let balance = ok!(t1.try_get_ref::<Tuple>(VmState::BALANCE_GLOBAL_IDX));
                value = ok!(balance.try_get_ref::<BigInt>(0).and_then(bigint_to_tokens));
                has_extra_currencies = balance.get(1).and_then(|v| v.as_cell()).is_some();
            } else if mode.contains(SendMsgFlags::WITH_REMAINING_BALANCE) {
                let balance = ok!(t1.try_get_ref::<Tuple>(VmState::IN_MSG_VALUE_GLOBAL_IDX));
                let msg_value = ok!(balance.try_get_ref::<BigInt>(0).and_then(bigint_to_tokens));
                value.try_add_assign(msg_value)?;
                has_extra_currencies |= balance.get(1).and_then(|v| v.as_cell()).is_some();
            }
        }

        // Compute fees and final message layout.
        let update_fees = |stats: CellTreeStats, fwd_fee: &mut Tokens, ihr_fee: &mut Tokens| {
            let fwd_fee_short = prices.compute_fwd_fee(stats);
            *fwd_fee = std::cmp::max(fwd_fee_short, user_fwd_fee);
            *ihr_fee = if ihr_disabled {
                Tokens::ZERO
            } else {
                std::cmp::max(
                    tokens_mul_frac(fwd_fee_short, prices.ihr_price_factor),
                    user_ihr_fee,
                )
            };
        };

        let compute_msg_root_bits =
            |msg_layout: &MessageLayout, fwd_fee: Tokens, ihr_fee: Tokens| {
                // Message info
                let mut bits = match &msg.info {
                    RelaxedMsgInfo::ExtOut(info) => {
                        2 + my_addr.range().size_bits() + ext_addr_bit_len(&info.dst) + 64 + 32
                    }
                    RelaxedMsgInfo::Int(info) => {
                        let fwd_fee_first = tokens_mul_frac(fwd_fee, prices.first_frac as _);
                        4 + my_addr.range().size_bits()
                            + info.dst.bit_len()
                            + ok!(tokens_bit_len(value))
                            + 1
                            + ok!(tokens_bit_len(fwd_fee - fwd_fee_first))
                            + ok!(tokens_bit_len(ihr_fee))
                            + 64
                            + 32
                    }
                };

                // State init
                bits += 1;
                if let Some(init) = &msg.init {
                    bits += 1 + if msg_layout.init_to_cell {
                        0
                    } else {
                        init.bit_len()
                    };
                }

                // Message body
                bits += 1;
                bits += if msg_layout.body_to_cell {
                    0
                } else {
                    msg.body.size_bits()
                };

                // Done
                Ok(bits)
            };
        let compute_msg_root_refs = |msg_layout: &MessageLayout| {
            let mut refs = match &msg.info {
                RelaxedMsgInfo::ExtOut(_) => 0,
                RelaxedMsgInfo::Int(_) => has_extra_currencies as usize,
            };

            // State init
            if let Some(init) = &msg.init {
                refs += if msg_layout.init_to_cell {
                    1
                } else {
                    init.reference_count() as usize
                }
            }

            // Body
            refs += if msg_layout.body_to_cell {
                1
            } else {
                msg.body.size_refs() as usize
            };

            // Done
            refs
        };

        let mut msg_layout = msg.layout.unwrap();

        // Compute fees for the initial layout.
        let mut fwd_fee = Tokens::ZERO;
        let mut ihr_fee = Tokens::ZERO;
        update_fees(stats, &mut fwd_fee, &mut ihr_fee);

        // Adjust layout for state init.
        if let Some(init) = &msg.init {
            if !msg_layout.init_to_cell
                && (ok!(compute_msg_root_bits(&msg_layout, fwd_fee, ihr_fee)) > cell::MAX_BIT_LEN
                    || compute_msg_root_refs(&msg_layout) > cell::MAX_REF_COUNT)
            {
                msg_layout.init_to_cell = true;
                stats.bit_count += init.bit_len() as u64;
                stats.cell_count += 1;
                update_fees(stats, &mut fwd_fee, &mut ihr_fee);
            }
        }

        // Adjust layout for body.
        if !msg_layout.body_to_cell
            && (ok!(compute_msg_root_bits(&msg_layout, fwd_fee, ihr_fee)) > cell::MAX_BIT_LEN
                || compute_msg_root_refs(&msg_layout) > cell::MAX_REF_COUNT)
        {
            // msg_layout.body_to_cell = true;
            stats.bit_count += msg.body.size_bits() as u64;
            stats.cell_count += 1;
            update_fees(stats, &mut fwd_fee, &mut ihr_fee);
        }

        // Push the total fee to the stack.
        ok!(stack.push_int(fwd_fee.into_inner().saturating_add(ihr_fee.into_inner())));

        // Done
        if send {
            drop(msg_cell);
            add_action(&mut st.cr, &mut st.gas, OutAction::SendMsg {
                mode,
                out_msg: Lazy::from_raw(Rc::unwrap_or_clone(raw_msg_cell)),
            })
        } else {
            Ok(0)
        }
    }
}

/// Returns a tuple of mode and `send` flag.
fn pop_send_msg_mode_ext(stack: &mut Stack) -> VmResult<(SendMsgFlags, bool)> {
    const DRY_RUN_BIT: u32 = 1 << 10;

    let mut raw_mode = ok!(stack.pop_smallint_range(0, 2047));
    let send = raw_mode & DRY_RUN_BIT == 0;
    raw_mode &= !DRY_RUN_BIT;
    vm_ensure!(raw_mode < 256, IntegerOutOfRange {
        min: 0,
        max: 255,
        actual: raw_mode.to_string(),
    });
    let mode = SendMsgFlags::from_bits_retain(raw_mode as u8);

    Ok((mode, send))
}

fn pop_change_library_mode(version: VmVersion, stack: &mut Stack) -> VmResult<ChangeLibraryMode> {
    let mode = if version.is_ton(4..) {
        let mode = ok!(stack.pop_smallint_range(0, 0b11111));
        // Check if flags match the allowed pattern
        vm_ensure!(mode & 0b1111 <= 2, IntegerOutOfRange {
            min: 0,
            max: 0b11111,
            actual: mode.to_string()
        });
        mode
    } else {
        ok!(stack.pop_smallint_range(0, 2))
    };

    Ok(ChangeLibraryMode::from_bits_retain(mode as u8))
}

fn parse_addr_workchain(addr: &OwnedCellSlice) -> VmResult<i32> {
    let mut cs = addr.apply()?;
    if !cs.load_bit()? {
        vm_bail!(IntegerOutOfRange {
            min: 1,
            max: 1,
            actual: "0".to_owned()
        })
    }

    let is_var = cs.load_bit()?;
    if cs.load_bit()? {
        // Read anycast if any.
        let depth = SplitDepth::new(load_uint_leq(&mut cs, 30)? as u8)?;
        cs.skip_first(depth.into_bit_len(), 0)?;
    }

    Ok(if is_var {
        cs.skip_first(9, 0)?; // Skip addr len
        cs.load_u32()? as i32
    } else {
        // NOTE: Cast to `i8` first to preserve the sign.
        cs.load_u8()? as i8 as i32
    })
}

fn ext_addr_bit_len(addr: &Option<ExtAddr>) -> u16 {
    match addr {
        Some(addr) => 2 + addr.bit_len(),
        None => 2,
    }
}

fn tokens_bit_len(value: Tokens) -> VmResult<u16> {
    let Some(bits) = value.bit_len() else {
        vm_bail!(IntegerOverflow);
    };
    Ok(bits)
}

fn bigint_to_tokens(int: &BigInt) -> VmResult<Tokens> {
    if let Some(int) = int.to_u128() {
        let int = Tokens::new(int);
        if int.is_valid() {
            return Ok(int);
        }
    }

    vm_bail!(IntegerOutOfRange {
        min: 0,
        max: Tokens::MAX.into_inner() as isize,
        actual: int.to_string(),
    })
}

fn tokens_mul_frac(value: Tokens, frac: u32) -> Tokens {
    Tokens::new(value.into_inner().saturating_mul(frac as u128) >> 16)
}

fn add_action(regs: &mut ControlRegs, gas: &mut GasConsumer, action: OutAction) -> VmResult<i32> {
    const ACTIONS_REG_IDX: usize = 5;
    let Some(c5) = regs.get_d(ACTIONS_REG_IDX) else {
        vm_bail!(ControlRegisterOutOfRange(ACTIONS_REG_IDX))
    };

    let actions_head = CellBuilder::build_from_ext((c5, action), gas.context())?;

    vm_log!("installing an output action");
    regs.set_d(ACTIONS_REG_IDX, actions_head);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::str::FromStr;

    use everscale_types::cell::{Cell, CellBuilder};
    use everscale_types::models::{Account, AccountState, OwnedMessage, StdAddr};
    use everscale_types::prelude::{Boc, CellFamily, Load};
    use everscale_vm::stack::Tuple;
    use num_bigint::BigInt;
    use tracing_test::traced_test;

    use crate::cont::OrdCont;
    use crate::stack::{RcStackValue, Stack};
    use crate::util::OwnedCellSlice;
    use crate::VmState;

    #[test]
    #[traced_test]
    fn send_msg_test() {
        let code = Boc::decode(&everscale_asm_macros::tvmasm! {
            r#"
            SETCP0 DUP IFNOTRET // return if recv_internal
            DUP
            PUSHINT 85143
            EQUAL OVER
            PUSHINT 78748
            EQUAL OR
            // "seqno" and "get_public_key" get-methods
            PUSHCONT {
                PUSHINT 1
                AND
                PUSHCTR c4 CTOS
                LDU 32
                LDU 32
                NIP
                PLDU 256
                CONDSEL
            }
            IFJMP
            // fail unless recv_external
            INC THROWIF 32

            PUSHPOW2 9 LDSLICEX // signature
            DUP
            LDU 32 // subwallet_id
            LDU 32 // valid_until
            LDU 32 // msg_seqno

            NOW
            XCHG s1, s3
            LEQ
            DUMPSTK
            THROWIF 35

            PUSH c4 CTOS
            LDU 32
            LDU 32
            LDU 256
            ENDS

            XCPU s3, s2
            EQUAL
            THROWIFNOT 33

            XCPU s4, s4
            EQUAL
            THROWIFNOT 34

            XCHG s0, s4
            HASHSU
            XC2PU s0, s5, s5
            CHKSIGNU THROWIFNOT 35

            ACCEPT

            PUSHCONT { DUP SREFS }
            PUSHCONT {
                LDU 8
                LDREF
                XCHG s0, s2
                SENDRAWMSG
            }
            WHILE

            ENDS SWAP INC

            NEWC
            STU 32
            STU 32
            STU 256
            ENDC
            POP c4
            "#
        })
        .unwrap();

        let code = OwnedCellSlice::from(code);

        let balance_tuple: Tuple = vec![Rc::new(BigInt::from(10000000000u64)), Stack::make_null()];

        let addr =
            StdAddr::from_str("0:4f4f10cb9a30582792fb3c1e364de5a6fbe6fe04f4167f1f12f83468c767aeb3")
                .unwrap();
        let addr = OwnedCellSlice::from(CellBuilder::build_from(addr).unwrap());

        let c7: Vec<RcStackValue> = vec![
            Rc::new(BigInt::from(0x076ef1ea)),
            Rc::new(BigInt::from(0)),                 // actions
            Rc::new(BigInt::from(0)),                 // msgs_sent
            Rc::new(BigInt::from(1732042729)),        // unix_time
            Rc::new(BigInt::from(55364288000000u64)), // block_logical_time
            Rc::new(BigInt::from(55396331000001u64)), // transaction_logical_time
            Rc::new(BigInt::from(0)),                 // rand_seed
            Rc::new(balance_tuple),
            Rc::new(addr),
            Stack::make_null(),
            Rc::new(code.clone()),
        ];

        let c4_data = Boc::decode_base64(
            "te6ccgEBAQEAKgAAUAAAAblLqS2KyLDWxgjLA6yhKJfmGLWfXdvRC34pWEXEek1ncgteNXU=",
        )
        .unwrap();

        let message_cell = Boc::decode_base64("te6ccgEBAgEAqQAB34gAnp4hlzRgsE8l9ng8bJvLTffN/AnoLP4+JfBo0Y7PXWYHO+2B5vPMosfjPalLE/qz0rm+wRn9g9sSu0q4Zwo0Lq5vB/YbhvWObr1T6jLdyEU3xEQ2uSP7sKARmIsEqMbIal1JbFM55wEgAAANyBwBAGhCACeniGXNGCwTyX2eDxsm8tN9838Cegs/j4l8GjRjs9dZodzWUAAAAAAAAAAAAAAAAAAA").unwrap();
        let message = OwnedMessage::load_from(
            &mut OwnedCellSlice::from(message_cell.clone()).apply().unwrap(),
        )
        .unwrap();
        let message_body = OwnedCellSlice::from(message.body);

        let stack: Vec<RcStackValue> = vec![
            Rc::new(BigInt::from(1406127106355u64)),
            Rc::new(BigInt::from(0)),
            Rc::new(message_cell),
            Rc::new(message_body),
            Rc::new(BigInt::from(-1)),
        ];

        let mut builder = VmState::builder();
        builder.c7 = Some(vec![Rc::new(c7)]);
        builder.stack = stack;
        builder.code = code;
        let mut vm_state = builder
            .with_gas_base(1000000)
            .with_gas_remaining(1000000)
            .with_gas_max(u64::MAX)
            .with_debug(TracingOutput::default())
            .build()
            .unwrap();
        vm_state.cr.set(4, Rc::new(c4_data)).unwrap();
        let result = vm_state.run();
        println!("code {result}");
    }

    #[test]
    #[traced_test]
    pub fn e_wallet_send_msg() {
        let code = Boc::decode_base64("te6cckEBBgEA/AABFP8A9KQT9LzyyAsBAgEgAgMABNIwAubycdcBAcAA8nqDCNcY7UTQgwfXAdcLP8j4KM8WI88WyfkAA3HXAQHDAJqDB9cBURO68uBk3oBA1wGAINcBgCDXAVQWdfkQ8qj4I7vyeWa++COBBwiggQPoqFIgvLHydAIgghBM7mRsuuMPAcjL/8s/ye1UBAUAmDAC10zQ+kCDBtcBcdcBeNcB10z4AHCAEASqAhSxyMsFUAXPFlAD+gLLaSLQIc8xIddJoIQJuZgzcAHLAFjPFpcwcQHLABLM4skB+wAAPoIQFp4+EbqOEfgAApMg10qXeNcB1AL7AOjRkzLyPOI+zYS/").unwrap();
        let code = OwnedCellSlice::from(code);

        let balance_tuple: Tuple = vec![Rc::new(BigInt::from(10000000000u64)), Stack::make_null()];

        let addr =
            StdAddr::from_str("0:6301b2c75596e6e569a6d13ae4ec70c94f177ece0be19f968ddce73d44e7afc7")
                .unwrap();
        let addr = OwnedCellSlice::from(CellBuilder::build_from(addr).unwrap());

        let c7: Vec<RcStackValue> = vec![
            Rc::new(BigInt::from(0x076ef1ea)),
            Rc::new(BigInt::from(0)),                 // actions
            Rc::new(BigInt::from(0)),                 // msgs_sent
            Rc::new(BigInt::from(1732048342)),        // unix_time
            Rc::new(BigInt::from(55398352000001u64)), // block_logical_time
            Rc::new(BigInt::from(55398317000004u64)), // transaction_logical_time
            Rc::new(BigInt::from(0)),                 // rand_seed
            Rc::new(balance_tuple),
            Rc::new(addr),
            Stack::make_null(),
            Rc::new(code.clone()),
        ];

        let c4_data = Boc::decode_base64(
            "te6ccgEBAQEAKgAAUMiw1sYIywOsoSiX5hi1n13b0Qt+KVhFxHpNZ3ILXjV1AAABk0YeykY=",
        )
        .unwrap();

        let message_cell = Boc::decode_base64("te6ccgEBBAEA0gABRYgAxgNljqstzcrTTaJ1ydjhkp4u/ZwXwz8tG7nOeonPX44MAQHhmt2/xQjjwjfYraY7Tv53Ct8o9OAtI8nD7DFB19TrG7W8wYMxQKtbXuvGvaKFoB9D0lMZwnPpZ1fEBWxaXZgtg/IsNbGCMsDrKEol+YYtZ9d29ELfilYRcR6TWdyC141dQAAAZNGIEb+Zzz2EEzuZGyACAWWADGA2WOqy3NytNNonXJ2OGSni79nBfDPy0buc56ic9fjgAAAAAAAAAAAAAAAHc1lAADgDAAA=").unwrap();
        let message = OwnedMessage::load_from(
            &mut OwnedCellSlice::from(message_cell.clone()).apply().unwrap(),
        )
        .unwrap();
        let message_body = OwnedCellSlice::from(message.body);

        let stack: Vec<RcStackValue> = vec![
            Rc::new(BigInt::from(4989195982u64)),
            Rc::new(BigInt::from(0)),
            Rc::new(message_cell),
            Rc::new(message_body),
            Rc::new(BigInt::from(-1)),
        ];

        let mut builder = VmState::builder();
        builder.c7 = Some(vec![Rc::new(c7)]);
        builder.stack = stack;
        builder.code = code;

        let mut vm_state = builder
            .with_gas_base(10000)
            .with_gas_remaining(10000)
            .with_gas_max(u64::MAX)
            .with_debug(TracingOutput::default())
            .build()
            .unwrap();
        vm_state.cr.set(4, Rc::new(c4_data)).unwrap();
        let result = vm_state.run();
        println!("code {result}");
    }

    #[test]
    #[traced_test]
    pub fn jetton() {
        let code = Boc::decode_base64("te6ccgECGgEABQ4AART/APSkE/S88sgLAQIBYgIDAgLLBAUCASAQEQHX0MtDTAwFxsI5EMIAg1yHTHwGCEBeNRRm6kTDhgEDXIfoAMO1E0PoA+kD6QNTU0/8B+GHRUEWhQTT4QchQBvoCUATPFljPFszMy//J7VTg+kD6QDH6ADH0AfoAMfoAATFw+DoC0x8BAdM/ARKBgAdojhkZYOA54tkgUGD+gvABPztRND6APpA+kDU1NP/Afhh0SaCEGQrfQe6jss1NVFhxwXy4EkE+kAh+kQwwADy4U36ANTRINDTHwGCEBeNRRm68uBIgEDXIfoA+kAx+kAx+gAg1wsAmtdLwAEBwAGw8rGRMOJUQxvgOSWCEHvdl9664wIlghAsdrlzuuMCNCQHCAkKAY4hkXKRceL4OSBuk4F4LpEg4iFulDGBfuCRAeJQI6gToHOBBK1w+DygAnD4NhKgAXD4NqBzgQUTghAJZgGAcPg3oLzysCVZfwsB5jUF+gD6QPgo+EEoEDQB2zxvIjD5AHB0yMsCygfL/8nQUAjHBfLgShKhRBRQNvhByFAG+gJQBM8WWM8WzMzL/8ntVPpA0SDXCwHAALOOIsiAEAHLBQHPFnD6AnABy2qCENUydtsByx8BAcs/yYBC+wCRW+IYAdI1XwM0AfpA0gABAdGVyCHPFsmRbeLIgBABywVQBM8WcPoCcAHLaoIQ0XNUAAHLH1AEAcs/I/pEMMAAjp34KPhBEDVBUNs8byIw+QBwdMjLAsoHy//J0BLPFpcxbBJwAcsB4vQAyYBQ+wAYBP6CEGUB81S6jiUwM1FCxwXy4EkC+kDRQAME+EHIUAb6AlAEzxZYzxbMzMv/ye1U4CSCEPuI4Rm6jiQxMwPRUTHHBfLgSYsCQDT4QchQBvoCUATPFljPFszMy//J7VTgJIIQy4YpArrjAjAjghAlCNZquuMCI4IQdDHyIbrjAhA2DA0ODwHAghA7msoAcPsC+Cj4QRA2QVDbPG8iMCD5AHB0yMsCygfL/8iAGAHLBQHPF1j6AgKYWHdQA8trzMyXMAFxWMtqzOLJgBH7AFAFoEMU+EHIUAb6AlAEzxZYzxbMzMv/ye1UGABONDZRRccF8uBJyFADzxbJEDQS+EHIUAb6AlAEzxZYzxbMzMv/ye1UACI2XwMCxwXy4EnU1NEB7VT7BABKM1BCxwXy4EkB0YsCiwJANPhByFAG+gJQBM8WWM8WzMzL/8ntVAAcXwaCENNyFYy63IQP8vACAUgSEwICcRYXAT+10V2omh9AH0gfSBqamn/gPww6IovgnwUfCCJbZ43kUBgCAWoUFQAuq1vtRND6APpA+kDU1NP/Afhh0RAkXwQALqpn7UTQ+gD6QPpA1NTT/wH4YdFfBfhBAVutvPaiaH0AfSB9IGpqaf+A/DDoii+CfBR8IIltnjeRGHyAODpkZYFlA+X/5OhAGACLrxb2omh9AH0gfSBqamn/gPww6L+Z6DbBeDhy69tRTZyXwoO38K5ReQKeK2EZw5RicZ5PRu2PdBPmLHgKOGRlg/oAZKGAQAH2hA9/cCb6RDGr+1MRSUYYBMjLA1AD+gIBzxYBzxbL/yCBAMrIyw8Bzxck+QAl12UlggIBNMjLFxLLD8sPy/+OKQakXAHLCXH5BABScAHL/3H5BACr+yiyUwS5kzQ0I5Ew4iDAICTAALEX5hAjXwMzMyJwA8sJySLIywESGQAU9AD0AMsAyQFvAg==").unwrap();
        let code = OwnedCellSlice::from(code);

        let balance_tuple: Tuple = vec![Rc::new(BigInt::from(1931553923u64)), Stack::make_null()];

        let addr =
            StdAddr::from_str("0:2a0c78148c73416b63250b990efdfbf9d5897bf3b33e2f5498a2fe0617174bb8")
                .unwrap();
        let addr = OwnedCellSlice::from(CellBuilder::build_from(addr).unwrap());

        let c7: Vec<RcStackValue> = vec![
            Rc::new(BigInt::from(0x076ef1ea)),
            Rc::new(BigInt::from(0)),                 // actions
            Rc::new(BigInt::from(0)),                 // msgs_sent
            Rc::new(BigInt::from(1733142533)),        // unix_time
            Rc::new(BigInt::from(50899537000013u64)), // block_logical_time
            Rc::new(BigInt::from(50899537000013u64)), // transaction_logical_time
            Rc::new(BigInt::from(0)),                 // rand_seed
            Rc::new(balance_tuple),
            Rc::new(addr.clone()),
            Stack::make_null(),
            // Rc::new(code.clone()),
        ];

        let c4_data = Boc::decode_base64(
            "te6ccgEBBAEA3gACTmE+QBlNGKCvtRVlwuLLP8LwzhcDJNm1TPewFBFqmlIYet7ln0NupwECCEICDvGeG/QPK6SS/KrDhu7KWb9oJ6OFBwjZ/NmttoOrwzYB5mh0dHBzOi8vZ2lzdC5naXRodWJ1c2VyY29udGVudC5jb20vRW1lbHlhbmVua29LLzI3MWMwYWRhMWRlNDJiOTdjNDU1YWM5MzVjOTcyZjQyL3Jhdy9iN2IzMGMzZTk3MGUwNzdlMTFkMDg1Y2M2NzEzYmUDADAzMTU3YzdjYTA4L21ldGFkYXRhLmpzb24=",
        )
            .unwrap();

        let stack: Vec<RcStackValue> = vec![
            Rc::new(addr),
            Rc::new(BigInt::from(103289)),
            // Rc::new(BigInt::from(106029)),
        ];

        let builder = VmState::builder();

        let mut vm_state = builder
            .with_c7(vec![Rc::new(c7)])
            .with_stack(stack)
            .with_code(code.clone())
            .with_gas_base(1000000)
            .with_gas_remaining(1000000)
            .with_gas_max(u64::MAX)
            .with_debug(TracingOutput::default())
            .build()
            .unwrap();
        vm_state.cr.set(4, Rc::new(c4_data)).unwrap();
        vm_state
            .cr
            .set(
                3,
                Rc::new(OrdCont::simple(
                    code.clone(),
                    crate::instr::codepage0().id(),
                )),
            )
            .unwrap();

        let result = !vm_state.run();
        println!("code {result}");
    }

    #[test]
    #[traced_test]
    pub fn gas_test() {
        let msg_cell = Boc::decode_base64("te6ccgEBAQEAWwAAsUgAMZM1//wnphAm4e74Ifiao3ipylccMDttQdF26orbI/8ABjJmv/+E9MIE3D3fBD8TVG8VOUrjhgdtqDou3VFbZH/QdzWUAAYUWGAAABZ6pIT8hMDDnIhA").unwrap();
        let new_slice = OwnedCellSlice::new(Cell::empty_cell());

        let account_cell = Boc::decode_base64("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").unwrap();
        let account = read_account(account_cell).unwrap();

        let (code, data) = match account.state {
            AccountState::Active(state) => (state.code.unwrap(), state.data.unwrap()),
            _ => panic!(),
        };

        let code_slice = OwnedCellSlice::from(code);

        let balance_tuple: Tuple = vec![
            Rc::new(BigInt::from(account.balance.tokens.into_inner())),
            Stack::make_null(),
        ];

        let addr = account.address.as_std().unwrap();
        let addr = OwnedCellSlice::from(CellBuilder::build_from(addr).unwrap());

        let c7: Vec<RcStackValue> = vec![
            Rc::new(BigInt::from(0x076ef1ea)),
            Rc::new(BigInt::from(0)),                 // actions
            Rc::new(BigInt::from(0)),                 // msgs_sent
            Rc::new(BigInt::from(1733142533)),        // unix_time
            Rc::new(BigInt::from(50899537000013u64)), // block_logical_time
            Rc::new(BigInt::from(50899537000013u64)), // transaction_logical_time
            Rc::new(BigInt::from(0)),                 // rand_seed
            Rc::new(balance_tuple),
            Rc::new(addr.clone()),
            Stack::make_null(),
            // Rc::new(code.clone()),
        ];

        let stack: Vec<RcStackValue> = vec![
            Rc::new(BigInt::from(1307493522)),
            Rc::new(BigInt::from(500000000)),
            Rc::new(msg_cell),
            Rc::new(new_slice),
            Rc::new(BigInt::from(0)),
        ];

        let builder = VmState::builder();
        let mut vm_state = builder
            .with_c7(vec![Rc::new(c7)])
            .with_stack(stack)
            .with_code(code_slice.clone())
            .with_gas_base(1000000)
            .with_gas_remaining(1000000)
            .with_gas_max(u64::MAX)
            .with_debug(TracingOutput::default())
            .build()
            .unwrap();
        vm_state.cr.set(4, Rc::new(data)).unwrap();
        vm_state
            .cr
            .set(
                3,
                Rc::new(OrdCont::simple(
                    code_slice.clone(),
                    crate::instr::codepage0().id(),
                )),
            )
            .unwrap();

        let result = !vm_state.run();
        println!("code {result}");
    }

    fn read_account(cell: Cell) -> Result<Box<Account>, everscale_types::error::Error> {
        let s = &mut cell.as_slice()?;
        assert!(s.load_bit()?);

        Ok(Box::new(Account {
            address: <_>::load_from(s)?,
            storage_stat: <_>::load_from(s)?,
            last_trans_lt: <_>::load_from(s)?,
            balance: <_>::load_from(s)?,
            state: <_>::load_from(s)?,
            init_code_hash: if s.is_data_empty() {
                None
            } else {
                Some(<_>::load_from(s)?)
            },
        }))
    }

    #[derive(Default)]
    struct TracingOutput {
        buffer: String,
    }

    impl std::fmt::Write for TracingOutput {
        fn write_str(&mut self, mut s: &str) -> std::fmt::Result {
            while !s.is_empty() {
                match s.split_once('\n') {
                    None => {
                        self.buffer.push_str(s);
                        return Ok(());
                    }
                    Some((prefix, rest)) => {
                        tracing::debug!("{}{prefix}", self.buffer);
                        self.buffer.clear();
                        s = rest;
                    }
                }
            }
            Ok(())
        }
    }
}
