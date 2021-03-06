address 0x0 {

module LibraTimestamp {
    use 0x0::Signer;
    use 0x0::Transaction;

    // A singleton resource holding the current Unix time in microseconds
    resource struct CurrentTimeMicroseconds {
        microseconds: u64,
    }

    // Initialize the global wall clock time resource.
    public fun initialize(association: &signer) {
        // Only callable by the Association address
        Transaction::assert(Signer::address_of(association) == 0xA550C18, 1);

        // TODO: Should the initialized value be passed in to genesis?
        let timer = CurrentTimeMicroseconds { microseconds: 0 };
        move_to(association, timer);
    }

    // Update the wall clock time by consensus. Requires VM privilege and will be invoked during block prologue.
    public fun update_global_time(
        account: &signer,
        proposer: address,
        timestamp: u64
    ) acquires CurrentTimeMicroseconds {
        // Can only be invoked by LibraVM privilege.
        Transaction::assert(Signer::address_of(account) == 0x0, 33);

        let global_timer = borrow_global_mut<CurrentTimeMicroseconds>(0xA550C18);
        if (proposer == 0x0) {
            // NIL block with null address as proposer. Timestamp must be equal.
            Transaction::assert(timestamp == global_timer.microseconds, 5001);
        } else {
            // Normal block. Time must advance
            Transaction::assert(global_timer.microseconds < timestamp, 5001);
        };
        global_timer.microseconds = timestamp;
    }

    // Get the timestamp representing `now` in microseconds.
    public fun now_microseconds(): u64 acquires CurrentTimeMicroseconds {
        borrow_global<CurrentTimeMicroseconds>(0xA550C18).microseconds
    }

    // Helper function to determine if the blockchain is at genesis state.
    public fun is_genesis(): bool acquires CurrentTimeMicroseconds {
        !::exists<CurrentTimeMicroseconds>(0xA550C18) || now_microseconds() == 0
    }
}

}
