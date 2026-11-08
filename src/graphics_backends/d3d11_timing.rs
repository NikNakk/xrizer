use super::GpuTiming;
use std::collections::VecDeque;
use windows::Win32::Graphics::Direct3D11::*;
use windows::core::Interface;

struct Queries {
    disjoint: ID3D11Query,
    start: ID3D11Query,
    end: ID3D11Query,
    frame: u32,
}

pub struct GpuTimer {
    context: ID3D11DeviceContext,
    free: Vec<Queries>,
    pending: VecDeque<Queries>,
    active: Option<Queries>,
}

impl GpuTimer {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Self {
        let mut free = Vec::new();
        for _ in 0..8 {
            let query = |kind| -> windows::core::Result<ID3D11Query> {
                let mut query = None;
                unsafe {
                    device.CreateQuery(
                        &D3D11_QUERY_DESC {
                            Query: kind,
                            MiscFlags: 0,
                        },
                        Some(&mut query),
                    )?;
                }
                query.ok_or_else(|| {
                    windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32))
                })
            };
            let queries = (|| {
                Ok::<_, windows::core::Error>(Queries {
                    disjoint: query(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                    start: query(D3D11_QUERY_TIMESTAMP)?,
                    end: query(D3D11_QUERY_TIMESTAMP)?,
                    frame: 0,
                })
            })();
            match queries {
                Ok(queries) => free.push(queries),
                Err(error) => {
                    log::warn!("D3D11 GPU timings unavailable: {error}");
                    break;
                }
            }
        }
        Self {
            context: context.clone(),
            free,
            pending: VecDeque::new(),
            active: None,
        }
    }

    pub fn begin(&mut self, frame: u32) {
        // Close an abandoned/loading frame without reusing an unresolved query.
        self.finish();
        let Some(mut queries) = self.free.pop() else {
            return;
        };
        queries.frame = frame;
        unsafe {
            self.context.Begin(&queries.disjoint);
            self.context.End(&queries.start);
        }
        self.active = Some(queries);
    }

    pub fn finish(&mut self) {
        let Some(queries) = self.active.take() else {
            return;
        };
        unsafe {
            self.context.End(&queries.end);
            self.context.End(&queries.disjoint);
        }
        self.pending.push_back(queries);
    }

    pub fn poll(&mut self) -> Vec<GpuTiming> {
        let mut complete = Vec::new();
        for _ in 0..self.pending.len() {
            let queries = self.pending.pop_front().unwrap();
            let mut frequency = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            let mut start = 0u64;
            let mut end = 0u64;
            let result = unsafe {
                let query = |q: &ID3D11Query, out: *mut std::ffi::c_void, bytes: u32| {
                    // GetData's Result<()> wrapper loses S_FALSE. Read the raw HRESULT.
                    (self.context.vtable().GetData)(
                        self.context.as_raw(),
                        q.as_raw(),
                        out,
                        bytes,
                        D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                    )
                };
                [
                    query(
                        &queries.disjoint,
                        (&mut frequency as *mut D3D11_QUERY_DATA_TIMESTAMP_DISJOINT).cast(),
                        std::mem::size_of_val(&frequency) as u32,
                    ),
                    query(&queries.start, (&mut start as *mut u64).cast(), 8),
                    query(&queries.end, (&mut end as *mut u64).cast(), 8),
                ]
            };
            if result.iter().any(|r| r.0 < 0) {
                // Invalid/device-lost query sets are retired, never overwritten in flight.
                continue;
            }
            if result.iter().any(|r| r.0 == 1) {
                self.pending.push_back(queries);
                continue;
            }
            if result.iter().all(|r| r.0 == 0)
                && !frequency.Disjoint.as_bool()
                && frequency.Frequency != 0
                && end >= start
            {
                complete.push(GpuTiming {
                    frame: queries.frame,
                    milliseconds: (end - start) as f64 * 1000.0 / frequency.Frequency as f64,
                });
            }
            self.free.push(queries);
        }
        complete
    }
}
