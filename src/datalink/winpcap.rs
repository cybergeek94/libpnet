// Copyright (c) 2014 Robert Clipsham <robert@octarineparrot.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate libc;

use std::cmp;
use std::collections::{RingBuf};
use std::ffi::CString;
use std::old_io::{IoResult, IoError};
use std::mem;
use std::raw::Slice;
use std::sync::Arc;

use bindings::{bpf, winpcap};
use datalink::{DataLinkChannelType};
use old_packet::Packet;
use old_packet::ethernet::{EthernetHeader, MutableEthernetHeader};
use util::NetworkInterface;

/// Evaluate the expression as a pointer and test if it is `NULL`.
/// If so, return `Err(IoError::last_error())`, the pointer otherwise.
macro_rules! try_get_ptr(
    ($get_ptr:expr) => (
        {
            let ptr = $get_ptr;
            if ptr.is_null() {
                return Err(IoError::last_error());
            }

            ptr
        }
    )
);

/// Evaluate the expression in an `unsafe` block as an integer and test if it is 0.
/// If so, return `Err(IoError::last_error())`.
macro_rules! try_ffi_unsafe {
    ($ffi_expr:expr) => (
        if unsafe { $ffi_expr } == 0 {
            return Err(IoError::last_error());
        }
    )
}

struct WinPcapAdapter {
    adapter: winpcap::LPADAPTER,
}

impl Drop for WinPcapAdapter {
    fn drop(&mut self) {
        if !self.adapter.is_null(){
            unsafe {
                winpcap::PacketCloseAdapter(self.adapter);
            }
        }
    }
}

struct WinPcapPacket {
    packet: winpcap::LPPACKET,
}

impl WinPcapPacket {
    fn with_buf(buf: &mut [u8]) -> IoResult<WinPcapPacket> {
        let packet = try_get_ptr!(unsafe { winpcap::PacketAllocatePacket() });

        unsafe {
            winpcap::PacketInitPacket(
                packet,
                buf.as_mut_ptr() as winpcap::PVOID,
                buf.len() as winpcap::UINT
            )
        }

        Ok(WinPcapPacket { 
            packet: packet,
        })
    }
}

impl Drop for WinPcapPacket {
    fn drop(&mut self) {
        if !self.packet.is_null() {
            unsafe {
                winpcap::PacketFreePacket(self.packet);
            }
        }
    }
}

pub fn datalink_channel(network_interface: &NetworkInterface,
           read_buffer_size: usize,
           write_buffer_size: usize,
           channel_type: DataLinkChannelType)
    -> IoResult<(DataLinkSenderImpl, DataLinkReceiverImpl)> {
    let mut read_buffer = Vec::new();
    read_buffer.resize(read_buffer_size, 0u8);

    let mut write_buffer = Vec::new();
    write_buffer.resize(write_buffer_size, 0u8);

    // Take advantage of RAII by creating this now.
    let adapter = WinPcapAdapter { 
        adapter: try_get_ptr!(unsafe {
            let net_if_str = CString::from_slice(network_interface.name.as_bytes());
            winpcap::PacketOpenAdapter(net_if_str.as_ptr() as *mut libc::c_char)
        }),
    };

    try_ffi_unsafe! {
        winpcap::PacketSetHwFilter(adapter.adapter, winpcap::NDIS_PACKET_TYPE_PROMISCUOUS)
    }

    // Set kernel buffer size
    try_ffi_unsafe! {
        winpcap::PacketSetBuff(adapter.adapter, read_buffer_size as libc::c_int)
    }

    // FIXME [windows] causes "os error 31: a device atteched to the system is not functioning"
    // FIXME [windows] This shouldn't be here - on Win32 reading seems to block indefinitely
    //       currently.
    /*
    try_ffi_unsafe! {
        winpcap::PacketSetReadTimeout(adapter, 1000)
    }
    */

    // Immediate mode
    try_ffi_unsafe! {
        winpcap::PacketSetMinToCopy(adapter.adapter, 1)
    }   

    let read_packet = try!(WinPcapPacket::with_buf(&mut *read_buffer));

    let write_packet = try!(WinPcapPacket::with_buf(&mut *write_buffer)); 

    let adapter = Arc::new(adapter);
    let sender = DataLinkSenderImpl {
        adapter: adapter.clone(),
        _vec: write_buffer,
        packet: write_packet,
    };
    let receiver = DataLinkReceiverImpl {
        adapter: adapter,
        _vec: read_buffer,
        packet: read_packet,
    };
    Ok((sender, receiver))
}


pub struct DataLinkSenderImpl {
    adapter: Arc<WinPcapAdapter>,
    _vec: Vec<u8>,
    packet: WinPcapPacket,
}

pub struct DataLinkReceiverImpl {
    adapter: Arc<WinPcapAdapter>,
    _vec: Vec<u8>,
    packet: WinPcapPacket,
}

impl DataLinkSenderImpl {
    pub fn build_and_send<F>(&mut self, num_packets: usize, packet_size: usize,
                          func: &mut F) -> Option<IoResult<()>>
        where F : FnMut(MutableEthernetHeader)
    {
        use std::raw::Slice;
        let len = num_packets * packet_size;
        if len >= unsafe { (*self.packet.packet).Length } as usize {
            None
        } else {
            let min = unsafe { cmp::min((*self.packet.packet).Length as usize, len) };
            let slice: &mut [u8] = unsafe {
                    mem::transmute(
                        Slice {
                            data: (*self.packet.packet).Buffer as *const (),
                            len: min
                        }
                    )
            };
            for chunk in slice.chunks_mut(packet_size) {
                {
                    let eh = MutableEthernetHeader::new(chunk);
                    func(eh);
                }

                // Make sure the right length of packet is sent
                let old_len = unsafe { (*self.packet.packet).Length };
                unsafe { (*self.packet.packet).Length = packet_size as u32; }

                let ret = unsafe { winpcap::PacketSendPacket(self.adapter.adapter, self.packet.packet, 0) };

                unsafe { (*self.packet.packet).Length = old_len; }

                match ret {
                    0 => return Some(Err(IoError::last_error())),
                    _ => ()
                }
            }
            Some(Ok(()))
        }
    }

    pub fn send_to(&mut self, packet: EthernetHeader, _dst: Option<NetworkInterface>)
        -> Option<IoResult<()>> {
        use old_packet::MutablePacket;
        self.build_and_send(1, packet.packet().len(), &mut |mut eh| {
            eh.clone_from(packet);
        })
    }
}

unsafe impl Send for DataLinkSenderImpl {}
unsafe impl Sync for DataLinkSenderImpl {}

impl DataLinkReceiverImpl {
    pub fn iter<'a>(&'a mut self) -> DataLinkChannelIteratorImpl<'a> {
        let buflen = unsafe { (*self.packet.packet).Length } as usize;
        DataLinkChannelIteratorImpl {
            pc: self,
            // Enough room for minimally sized packets without reallocating
            packets: RingBuf::with_capacity(buflen / 64)
        }
    }
}

unsafe impl Send for DataLinkReceiverImpl {}
unsafe impl Sync for DataLinkReceiverImpl {}

pub struct DataLinkChannelIteratorImpl<'a> {
    pc: &'a mut DataLinkReceiverImpl,
    packets: RingBuf<(usize, usize)>,
}

impl<'a> DataLinkChannelIteratorImpl<'a> {
    pub fn next<'c>(&'c mut self) -> IoResult<EthernetHeader<'c>> {
        // NOTE Most of the logic here is identical to FreeBSD/OS X
        if self.packets.is_empty() {
            let ret = unsafe {
                winpcap::PacketReceivePacket(self.pc.adapter.adapter, self.pc.packet.packet, 0)
            };
            let buflen = match ret {
                0 => return Err(IoError::last_error()),
                _ => unsafe { (*self.pc.packet.packet).ulBytesReceived },
            };
            let mut ptr = unsafe { (*self.pc.packet.packet).Buffer };
            let end = unsafe { (*self.pc.packet.packet).Buffer.offset(buflen as isize) };
            while ptr < end {
                unsafe {
                    let packet: *const bpf::bpf_hdr = mem::transmute(ptr);
                    let start = ptr as isize +
                                (*packet).bh_hdrlen as isize -
                                (*self.pc.packet.packet).Buffer as isize;
                    self.packets.push_back((start as usize, (*packet).bh_caplen as usize));
                    let offset = (*packet).bh_hdrlen as isize + (*packet).bh_caplen as isize;
                    ptr = ptr.offset(bpf::BPF_WORDALIGN(offset));
                }
            }
        }
        let (start, len) = self.packets.pop_front().unwrap();
        let slice = unsafe {
            let data = (*self.pc.packet.packet).Buffer as usize + start;
            mem::transmute(Slice { data: data as *const u8, len: len } )
        };
        Ok(EthernetHeader::new(slice))
    }
}

