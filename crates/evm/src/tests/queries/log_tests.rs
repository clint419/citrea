use std::str::FromStr;

use alloy_primitives::FixedBytes;
use hex::FromHex;
use reth_primitives::constants::ETHEREUM_BLOCK_GAS_LIMIT;
use reth_primitives::BlockNumberOrTag;
use revm::primitives::{B256, U256};
use sov_modules_api::default_context::DefaultContext;
use sov_modules_api::utils::generate_address;
use sov_modules_api::{Context, Module, StateVecAccessor};

use crate::call::CallMessage;
use crate::smart_contracts::LogsContract;
use crate::tests::call_tests::{create_contract_message, get_evm_config, publish_event_message};
use crate::tests::genesis_tests::get_evm;
use crate::tests::queries::init_evm;
use crate::{EthApiError, Filter, FilterBlockOption, FilterSet};

type C = DefaultContext;

#[test]
fn logs_for_filter_test() {
    let (evm, mut working_set, _) = init_evm();

    let result = evm.eth_get_logs(
        Filter {
            block_option: FilterBlockOption::AtBlockHash(B256::from([1u8; 32])),
            address: FilterSet::default(),
            topics: [
                FilterSet::default(),
                FilterSet::default(),
                FilterSet::default(),
                FilterSet::default(),
            ],
        },
        &mut working_set,
    );

    assert_eq!(result, Err(EthApiError::UnknownBlockNumber.into()));

    let available_res = evm.eth_get_logs(
        Filter {
            block_option: FilterBlockOption::AtBlockHash(
                FixedBytes::from_hex(
                    "0x463f932c9ef1c01a59f2495ddcb7ae16d1a4afc2b5f38998486c4bf16cc94a76",
                )
                .unwrap(),
            ),
            address: FilterSet::default(),
            topics: [
                FilterSet::default(),
                FilterSet::default(),
                FilterSet::default(),
                FilterSet::default(),
            ],
        },
        &mut working_set,
    );

    // TODO: Check this better.
    assert_eq!(available_res.unwrap().len(), 8);
}

#[test]
fn log_filter_test_at_block_hash() {
    let (config, dev_signer, contract_addr) =
        get_evm_config(U256::from_str("100000000000000000000").unwrap(), None);

    let (evm, mut working_set) = get_evm(&config);

    evm.begin_soft_confirmation_hook([5u8; 32], [42u8; 32], &[10u8; 32], 1, &mut working_set);
    {
        let sender_address = generate_address::<C>("sender");
        let sequencer_address = generate_address::<C>("sequencer");
        let context = C::new(sender_address, sequencer_address, 1);

        // deploy logs contract
        // call the contract function
        // the last topic will be Keccak256("hello")
        // call the contract function
        // the last topic will be Keccak256("hi")
        let rlp_transcations = vec![
            create_contract_message(&dev_signer, 0, LogsContract::default()),
            publish_event_message(contract_addr, &dev_signer, 1, "hello".to_string()),
            publish_event_message(contract_addr, &dev_signer, 2, "hi".to_string()),
        ];

        evm.call(
            CallMessage {
                txs: rlp_transcations,
            },
            &context,
            &mut working_set,
        )
        .unwrap();
    }
    evm.end_soft_confirmation_hook(&mut working_set);
    evm.finalize_hook(&[99u8; 32].into(), &mut working_set.accessory_state());

    // `AnotherLog` topics
    // [0xf16dfb875e436384c298237e04527f538a5eb71f60593cfbaae1ff23250d22a9, event signature => (kecccak256("AnotherLog(address)")
    //  0x000000000000000000000000819c5497b157177315e1204f52e588b393771719]

    // `Log`topics
    // [0xa9943ee9804b5d456d8ad7b3b1b975a5aefa607e16d13936959976e776c4bec7, event signature => (keccak256("Log(address,address,string,string)"))
    //  0x0000000000000000000000009e1abd37ec34bbc688b6a2b7d9387d9256cf1773,
    //  0x000000000000000000000000819c5497b157177315e1204f52e588b393771719,
    //  0x1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8 or 0x7624778dedc75f8b322b9fa1632a610d40b85e106c7d9bf0e743a9ce291b9c6f] (keccak256("hello") or keccak256("hi"))

    /*
       A transaction with a log with topics [A, B] will be matched by the following topic filters:
       1) [] “anything”
       2) [A] “A in first position (and anything after)”
       3) [null, B] “anything in first position AND B in second position (and anything after)”
       4) [A, B] “A in first position AND B in second position (and anything after)”
       5) [[A, B], [A, B]] “(A OR B) in first position AND (A OR B) in second position (and anything after)”
    */

    let block = evm.blocks.last(&mut working_set.accessory_state()).unwrap();
    let mut address = FilterSet::default();
    // Test without address and topics
    let mut topics: [FilterSet<B256>; 4] = [
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
    ];

    let filter = Filter {
        block_option: crate::FilterBlockOption::AtBlockHash(block.header.hash()),
        address: address.clone(),
        topics: topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();
    // should get all the logs
    assert_eq!(rpc_logs.len(), 4);

    // with address and without topics
    address.0.insert(contract_addr);

    let filter = Filter {
        block_option: crate::FilterBlockOption::AtBlockHash(block.header.hash()),
        address: address.clone(),
        topics: topics.clone(),
    };
    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();
    // 1) should get all the logs
    assert_eq!(rpc_logs.len(), 4);

    let empty_topic: FilterSet<B256> = FilterSet::default();

    let mut sig_topic = FilterSet::default();
    sig_topic.0.insert(B256::from_slice(
        hex::decode("a9943ee9804b5d456d8ad7b3b1b975a5aefa607e16d13936959976e776c4bec7")
            .unwrap()
            .as_slice(),
    ));

    topics[0] = sig_topic.clone();

    let filter = Filter {
        block_option: crate::FilterBlockOption::AtBlockHash(block.header.hash()),
        address: address.clone(),
        topics: topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();

    // 2) should get the logs with the signature
    assert_eq!(rpc_logs.len(), 2);

    let mut last_topic = FilterSet::default();
    last_topic.0.insert(B256::from_slice(
        hex::decode("1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8")
            .unwrap()
            .as_slice(),
    ));
    topics[0] = empty_topic;
    topics[3] = last_topic.clone();

    let filter = Filter {
        block_option: crate::FilterBlockOption::AtBlockHash(block.header.hash()),
        address: address.clone(),
        topics: topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();

    // 3) should get only the first log with hello as message
    assert_eq!(rpc_logs.len(), 1);
    assert_eq!(
        hex::encode(rpc_logs[0].topics[3]).to_string(),
        "1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8"
    );

    last_topic.0.insert(B256::from_slice(
        hex::decode("7624778dedc75f8b322b9fa1632a610d40b85e106c7d9bf0e743a9ce291b9c6f")
            .unwrap()
            .as_slice(),
    ));
    topics[3] = last_topic.clone();

    let filter = Filter {
        block_option: crate::FilterBlockOption::AtBlockHash(block.header.hash()),
        address: address.clone(),
        topics: topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();

    // 3) should get the logs with hello and hi messages
    assert_eq!(rpc_logs.len(), 2);

    topics[0] = sig_topic.clone();
    topics[3].0.remove(&B256::from_slice(
        hex::decode("7624778dedc75f8b322b9fa1632a610d40b85e106c7d9bf0e743a9ce291b9c6f")
            .unwrap()
            .as_slice(),
    ));

    let filter = Filter {
        block_option: crate::FilterBlockOption::AtBlockHash(block.header.hash()),
        address: address.clone(),
        topics: topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();

    // 4) should get the logs with given signature and hello message
    assert_eq!(rpc_logs.len(), 1);

    // add the signature of anotherlog to the first topic set
    topics[0].0.insert(B256::from_slice(
        hex::decode("f16dfb875e436384c298237e04527f538a5eb71f60593cfbaae1ff23250d22a9")
            .unwrap()
            .as_slice(),
    ));
    // add the hi topic to the last topic set
    topics[3].0.insert(B256::from_slice(
        hex::decode("7624778dedc75f8b322b9fa1632a610d40b85e106c7d9bf0e743a9ce291b9c6f")
            .unwrap()
            .as_slice(),
    ));

    let filter = Filter {
        block_option: crate::FilterBlockOption::AtBlockHash(block.header.hash()),
        address: address.clone(),
        topics: topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();

    // 5) should get the logs with given signatures and hello or hi messages, so in this case all logs with messages
    assert_eq!(rpc_logs.len(), 2);
}

#[test]
fn log_filter_test_with_range() {
    let (config, dev_signer, contract_addr) =
        get_evm_config(U256::from_str("100000000000000000000").unwrap(), None);

    let (evm, mut working_set) = get_evm(&config);

    evm.begin_soft_confirmation_hook([5u8; 32], [42u8; 32], &[10u8; 32], 1, &mut working_set);
    {
        let sender_address = generate_address::<C>("sender");
        let sequencer_address = generate_address::<C>("sequencer");
        let context = C::new(sender_address, sequencer_address, 1);

        // deploy selfdestruct contract
        // call the contract function
        // the last topic will be Keccak256("hello")
        // call the contract function
        // the last topic will be Keccak256("hi")
        let rlp_transactions = vec![
            create_contract_message(&dev_signer, 0, LogsContract::default()),
            publish_event_message(contract_addr, &dev_signer, 1, "hello".to_string()),
            publish_event_message(contract_addr, &dev_signer, 2, "hi".to_string()),
        ];

        evm.call(
            CallMessage {
                txs: rlp_transactions,
            },
            &context,
            &mut working_set,
        )
        .unwrap();
    }
    evm.end_soft_confirmation_hook(&mut working_set);
    evm.finalize_hook(&[99u8; 32].into(), &mut working_set.accessory_state());

    // Test with block range from start to finish, should get all logs
    let empty_topics = [
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
    ];
    let filter = Filter {
        block_option: crate::FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Earliest),
            to_block: Some(BlockNumberOrTag::Latest),
        },
        address: FilterSet::default(),
        topics: empty_topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();

    assert_eq!(rpc_logs.len(), 4);

    evm.begin_soft_confirmation_hook([5u8; 32], [42u8; 32], &[99u8; 32], 1, &mut working_set);
    {
        let sender_address = generate_address::<C>("sender");
        let sequencer_address = generate_address::<C>("sequencer");
        let context = C::new(sender_address, sequencer_address, 1);
        // call the contract function
        evm.call(
            CallMessage {
                txs: vec![publish_event_message(
                    contract_addr,
                    &dev_signer,
                    3,
                    "message".to_string(),
                )],
            },
            &context,
            &mut working_set,
        )
        .unwrap();
        // the last topic will be Keccak256("message")
    }
    evm.end_soft_confirmation_hook(&mut working_set);
    evm.finalize_hook(&[100u8; 32].into(), &mut working_set.accessory_state());
    let filter = Filter {
        block_option: crate::FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Latest),
            to_block: Some(BlockNumberOrTag::Latest),
        },
        address: FilterSet::default(),
        topics: empty_topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set).unwrap();
    // In the last block we have 2 logs
    assert_eq!(rpc_logs.len(), 2);
}

#[test]
fn test_log_limits() {
    // citrea::initialize_logging();

    // bigger block is needed to be able to include all the transactions
    let (config, dev_signer, contract_addr) = get_evm_config(
        U256::from_str("100000000000000000000").unwrap(),
        Some(20 * ETHEREUM_BLOCK_GAS_LIMIT),
    );

    let (evm, mut working_set) = get_evm(&config);

    evm.begin_soft_confirmation_hook([5u8; 32], [42u8; 32], &[10u8; 32], 1, &mut working_set);
    {
        let sender_address = generate_address::<C>("sender");
        let sequencer_address = generate_address::<C>("sequencer");
        let context = C::new(sender_address, sequencer_address, 1);

        // deploy logs contract
        let mut rlp_transactions = vec![create_contract_message(
            &dev_signer,
            0,
            LogsContract::default(),
        )];

        // call the contracts 10_001 times so we got 20_002 logs (response limit is 20_000)
        for i in 0..10001 {
            rlp_transactions.push(publish_event_message(
                contract_addr,
                &dev_signer,
                i + 1,
                "hello".to_string(),
            ));
        }

        // deploy logs contract
        let mut rlp_transactions = vec![create_contract_message(
            &dev_signer,
            0,
            LogsContract::default(),
        )];

        // call the contracts 10_001 times so we got 20_002 logs (response limit is 20_000)
        for i in 0..10001 {
            rlp_transactions.push(publish_event_message(
                contract_addr,
                &dev_signer,
                i + 1,
                "hello".to_string(),
            ));
        }

        evm.call(
            CallMessage {
                txs: rlp_transactions,
            },
            &context,
            &mut working_set,
        )
        .unwrap();
    }
    evm.end_soft_confirmation_hook(&mut working_set);
    evm.finalize_hook(&[99u8; 32].into(), &mut working_set.accessory_state());

    // Test with block range from start to finish, should get all logs
    let empty_topics = [
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
    ];
    let filter = Filter {
        block_option: crate::FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Earliest),
            to_block: Some(BlockNumberOrTag::Latest),
        },
        address: FilterSet::default(),
        topics: empty_topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set);

    assert!(rpc_logs.is_err());
    if let Err(rpc_err) = rpc_logs {
        assert_eq!(
            rpc_err.message(),
            "query exceeds max results 20000".to_string()
        );
    }

    // Test with block range from start to finish, should get all logs
    let empty_topics = [
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
        FilterSet::default(),
    ];

    for _ in 1..100_001 {
        // generate 100_000 blocks to test the max block range limit
        evm.begin_soft_confirmation_hook([5u8; 32], [42u8; 32], &[99u8; 32], 1, &mut working_set);
        evm.end_soft_confirmation_hook(&mut working_set);
        evm.finalize_hook(&[99u8; 32].into(), &mut working_set.accessory_state());
    }

    let filter = Filter {
        block_option: crate::FilterBlockOption::Range {
            from_block: Some(BlockNumberOrTag::Number(1)),
            to_block: Some(BlockNumberOrTag::Number(100_001)),
        },
        address: FilterSet::default(),
        topics: empty_topics.clone(),
    };

    let rpc_logs = evm.eth_get_logs(filter, &mut working_set);

    assert!(rpc_logs.is_err());
    assert_eq!(
        rpc_logs.err().unwrap().message(),
        "query exceeds max block range 100000".to_string()
    );
}
