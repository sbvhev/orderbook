use borsh::{BorshDeserialize, BorshSerialize};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};
use solana_program::{account_info::AccountInfo, pubkey::Pubkey};
use std::{cell::RefCell, convert::TryInto, io::Write, mem::size_of, rc::Rc};

use crate::{critbit::IoError, error::AoError, orderbook::ORDER_SUMMARY_SIZE};

#[derive(BorshDeserialize, BorshSerialize, Clone, Debug)]
pub enum AccountTag {
    Initialized,
    Market,
    EventQueue,
    Bids,
    Asks,
}

#[derive(BorshDeserialize, BorshSerialize, Clone, Copy, PartialEq, FromPrimitive, ToPrimitive)]
#[repr(u8)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

#[derive(BorshDeserialize, BorshSerialize, Clone, PartialEq)]
pub enum SelfTradeBehavior {
    DecrementTake,
    CancelProvide,
    AbortTransaction,
}

#[derive(BorshDeserialize, BorshSerialize)]
pub struct MarketState {
    pub tag: AccountTag,
    pub caller_authority: Pubkey, // The program that consumes the event queue via CPIs
    pub event_queue: Pubkey,
    pub bids: Pubkey,
    pub asks: Pubkey,
    pub callback_info_len: u64,
    //TODO cranked_accs
}

////////////////////////////////////////////////////
// Events
#[derive(BorshDeserialize, BorshSerialize)]
pub enum Event {
    Fill {
        taker_side: Side,
        maker_order_id: u128,
        quote_size: u64,
        asset_size: u64,
        maker_callback_info: Vec<u8>,
        taker_callback_info: Vec<u8>,
    },
    Out {
        side: Side,
        order_id: u128,
        asset_size: u64,
        callback_info: Vec<u8>,
    },
}

impl Event {
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<(), IoError> {
        match self {
            Event::Fill {
                taker_side,
                maker_order_id,
                quote_size,
                asset_size,
                maker_callback_info,
                taker_callback_info,
            } => {
                writer.write_all(&[taker_side.to_u8().unwrap()])?;
                writer.write_all(&maker_order_id.to_le_bytes())?;
                writer.write_all(&quote_size.to_le_bytes())?;
                writer.write_all(&asset_size.to_le_bytes())?;
                writer.write_all(&maker_callback_info)?;
                writer.write_all(&taker_callback_info)?;
            }
            Event::Out {
                side,
                order_id,
                asset_size,
                callback_info,
            } => {
                writer.write_all(&[side.to_u8().unwrap()])?;
                writer.write_all(&order_id.to_le_bytes())?;
                writer.write_all(&asset_size.to_le_bytes())?;
                writer.write_all(&callback_info)?;
            }
        };
        Ok(())
    }

    pub fn deserialize(buf: &mut &[u8], callback_info_len: usize) -> Self {
        match buf[0] {
            0 => Event::Fill {
                taker_side: Side::from_u8(buf[1]).unwrap(),
                maker_order_id: u128::from_le_bytes(buf[2..18].try_into().unwrap()),
                quote_size: u64::from_le_bytes(buf[18..26].try_into().unwrap()),
                asset_size: u64::from_le_bytes(buf[26..34].try_into().unwrap()),
                maker_callback_info: buf[34..34 + callback_info_len].to_owned(),
                taker_callback_info: buf[34 + callback_info_len..34 + (callback_info_len << 1)]
                    .to_owned(),
            },
            1 => unimplemented!(),
            _ => unreachable!(),
        }
    }
}

////////////////////////////////////////////////////
// Event Queue

#[derive(BorshDeserialize, BorshSerialize, Clone)]
pub struct EventQueueHeader {
    tag: AccountTag, // Initialized, EventQueue
    head: u64,
    count: u64,
    event_size: u64,
    seq_num: u64,
    register_size: u32,
}
pub const EVENT_QUEUE_HEADER_LEN: usize = size_of::<EventQueueHeader>();

impl Default for EventQueueHeader {
    fn default() -> Self {
        Self {
            tag: AccountTag::EventQueue,
            head: 0,
            count: 0,
            event_size: 0,
            register_size: ORDER_SUMMARY_SIZE + 1,
            seq_num: 0,
        }
    }
}

pub struct EventQueue<'a> {
    // The event queue account contains a serialized header
    // and a circular buffer of serialized events
    pub(crate) header: EventQueueHeader,
    pub(crate) buffer: Rc<RefCell<&'a mut [u8]>>, //The whole account data
    pub(crate) callback_info_len: usize,
}

#[derive(BorshDeserialize, BorshSerialize)]
pub enum Register<T: BorshSerialize + BorshDeserialize> {
    Uninitialized,
    Initialized(T),
}

impl<T: BorshSerialize + BorshDeserialize> Register<T> {
    pub fn unwrap(self) -> T {
        if let Self::Initialized(a) = self {
            a
        } else {
            panic!()
        }
    }
}

impl<'a> EventQueue<'a> {
    pub fn new_safe(
        header: EventQueueHeader,
        account: &AccountInfo<'a>,
        callback_info_len: usize,
    ) -> Self {
        let q = Self {
            header,
            buffer: Rc::clone(&account.data),
            callback_info_len,
        };
        q.clear_register();
        q
    }

    pub fn new(
        header: EventQueueHeader,
        account: Rc<RefCell<&'a mut [u8]>>,
        callback_info_len: usize,
    ) -> Self {
        Self {
            header,
            buffer: account,
            callback_info_len,
        }
    }
}

impl EventQueue<'_> {
    pub fn gen_order_id(&mut self, limit_price: u64, side: Side) -> u128 {
        let seq_num = self.gen_seq_num();
        let upper = (limit_price as u128) << 64;
        let lower = match side {
            Side::Bid => !seq_num,
            Side::Ask => seq_num,
        };
        upper | (lower as u128)
    }

    fn gen_seq_num(&mut self) -> u64 {
        let seq_num = self.header.seq_num;
        self.header.seq_num += 1;
        seq_num
    }

    pub fn get_buf_len(&self) -> usize {
        self.buffer.borrow().len() - EVENT_QUEUE_HEADER_LEN - (self.header.register_size as usize)
    }

    pub fn full(&self) -> bool {
        self.header.count as usize == (self.get_buf_len() / (self.header.event_size as usize))
        //TODO check
    }

    pub fn push_back(&mut self, event: Event) -> Result<(), Event> {
        if self.full() {
            return Err(event);
        }
        let offset = EVENT_QUEUE_HEADER_LEN
            + (((self.header.register_size as u64)
                + self.header.head
                + self.header.count * self.header.event_size) as usize)
                % self.get_buf_len();
        let mut queue_event_data =
            &mut self.buffer.borrow_mut()[offset..offset + (self.header.event_size as usize)];
        event.serialize(&mut queue_event_data).unwrap();

        self.header.count += 1;
        self.header.seq_num += 1;

        Ok(())
    }

    pub fn peek_front(&self) -> Option<Event> {
        if self.header.count == 0 {
            return None;
        }
        let offset = EVENT_QUEUE_HEADER_LEN
            + ((self.header.register_size as u64) + self.header.head) as usize;
        let mut event_data =
            &self.buffer.borrow()[offset..offset + (self.header.event_size as usize)];
        Some(Event::deserialize(&mut event_data, self.callback_info_len))
    }

    pub fn pop_front(&mut self) -> Result<Event, AoError> {
        if self.header.count == 0 {
            return Err(AoError::EventQueueEmpty);
        }
        let offset = EVENT_QUEUE_HEADER_LEN
            + ((self.header.register_size as u64) + self.header.head) as usize;
        let mut event_data =
            &self.buffer.borrow()[offset..offset + (self.header.event_size as usize)];
        let event = Event::deserialize(&mut event_data, self.callback_info_len);

        self.header.count -= 1;
        self.header.head = (self.header.head + 1) % self.get_buf_len() as u64;

        Ok(event)
    }

    pub fn pop_n(&mut self, number_of_entries_to_pop: u64) {
        let capped_number_of_entries_to_pop =
            std::cmp::min(self.header.count, number_of_entries_to_pop);
        self.header.count -= capped_number_of_entries_to_pop;
        self.header.head =
            (self.header.head + capped_number_of_entries_to_pop) % self.get_buf_len() as u64;
    }

    pub fn write_to_register<T: BorshSerialize + BorshDeserialize>(&self, object: T) {
        let mut register = &mut self.buffer.borrow_mut()
            [EVENT_QUEUE_HEADER_LEN..EVENT_QUEUE_HEADER_LEN + (self.header.register_size as usize)];
        Register::Initialized(object)
            .serialize(&mut register)
            .unwrap();
    }

    pub fn clear_register(&self) {
        let mut register = &mut self.buffer.borrow_mut()
            [EVENT_QUEUE_HEADER_LEN..EVENT_QUEUE_HEADER_LEN + (self.header.register_size as usize)];
        Register::<u8>::Uninitialized
            .serialize(&mut register)
            .unwrap();
    }

    pub fn read_register<T: BorshSerialize + BorshDeserialize>(
        &self,
    ) -> Result<Register<T>, IoError> {
        let mut register = &self.buffer.borrow()
            [EVENT_QUEUE_HEADER_LEN..EVENT_QUEUE_HEADER_LEN + (self.header.register_size as usize)];
        Register::deserialize(&mut register)
    }

    // #[inline]
    // pub fn revert_pushes(&mut self, desired_len: u64) -> DexResult<()> {
    //     check_assert!(desired_len <= self.header.count())?;
    //     let len_diff = self.header.count() - desired_len;
    //     self.header.set_count(desired_len);
    //     self.header.decr_event_id(len_diff);
    //     Ok(())
    // }

    // pub fn iter(&self) -> impl Iterator<Item = &H::Item> {
    //     QueueIterator {
    //         queue: self,
    //         index: 0,
    //     }
    // }
}
