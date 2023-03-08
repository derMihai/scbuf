# scbuf
`scbuf` (sync circular buffer) is a `Send` and `Sync`, lock-free circular buffer 
implementation.  

In the single-producer, single-consumer scenario, it relies solely on atomics for 
synchronization. See `new_scbuf` for multiple-producer/consumer usage.