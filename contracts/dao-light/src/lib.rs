#![no_std]
use codec::{Decode, Encode};
pub use dao_io::*;
use gstd::{exec, msg, prelude::*, ActorId, String};
use scale_info::TypeInfo;
pub mod state;
use state::*;
pub mod ft_messages;
pub use ft_messages::*;
const ZERO_ID: ActorId = ActorId::new([0u8; 32]);

#[derive(Debug, Default)]
struct Dao {
    admin: ActorId,
    approved_token_program_id: ActorId,
    period_duration: u64,
    voting_period_length: u64,
    grace_period_length: u64,
    total_shares: u128,
    members: BTreeMap<ActorId, Member>,
    proposal_id: u128,
    proposals: BTreeMap<u128, Proposal>,
}

#[derive(Debug, Default, Clone, Decode, Encode, TypeInfo)]
pub struct Proposal {
    pub proposer: ActorId,
    pub applicant: ActorId,
    pub yes_votes: u128,
    pub no_votes: u128,
    pub quorum: u128,
    pub amount: u128,
    pub processed: bool,
    pub did_pass: bool,
    pub cancelled: bool,
    pub details: String,
    pub starting_period: u64,
    pub ended_at: u64,
    pub votes_by_member: BTreeMap<ActorId, Vote>,
}

#[derive(Debug, Clone, Encode, Decode, TypeInfo)]
pub struct Member {
    pub shares: u128,
    pub highest_index_yes_vote: u128,
}

static mut DAO: Option<Dao> = None;

impl Dao {
     
    async fn deposit(&mut self, amount: u128) {       
        transfer_tokens(&self.approved_token_program_id, &msg::source(), &exec::program_id(), amount).await;
        self.members.entry(msg::source())
            .and_modify(|member| { member.shares += amount })
            .or_insert(Member {
                    shares: amount,
                    highest_index_yes_vote: 0,
                });
    }

    /// The proposal of funding
    /// Requirements:
    /// * The proposal can be submitted only by the existing members or their delegate addresses
    /// * The receiver ID can't be the zero
    /// * The DAO must have enough funds to finance the proposal
    /// Arguments:
    /// * `receiver`: an actor that will be funded
    /// * `amount`: the number of ERC20 tokens that will be sent to the receiver
    /// * `quorum`: a certain threshold of YES votes in order for the proposal to pass
    /// * `details`: the proposal description
    async fn submit_funding_proposal(
        &mut self,
        applicant: &ActorId,
        amount: u128,
        quorum: u128,
        details: String,
    ) {
        self.check_for_membership();

        if applicant == &ZERO_ID {
            panic!("Proposal for the zero address");
        }

        // check that DAO has sufficient funds
        let balance = balance(&self.approved_token_program_id, &exec::program_id()).await;
        if balance < amount {
            panic!("Not enough funds in DAO");
        }

        let mut starting_period = exec::block_timestamp();
        // compute startingPeriod for proposal
        // there should be a minimum time interval between proposals (period_duration) so that members have time to ragequit
        if self.proposal_id > 0 {
            let previous_starting_period = self
                .proposals
                .get(&(&self.proposal_id - 1))
                .unwrap()
                .starting_period;
            if starting_period < previous_starting_period + self.period_duration {
                starting_period = previous_starting_period + self.period_duration;
            }
        }

        let proposal = Proposal {
            proposer: msg::source(),
            applicant: *applicant,
            quorum,
            amount,
            details,
            starting_period,
            ended_at: starting_period + self.voting_period_length,
            ..Proposal::default()
        };

        self.proposals.insert(self.proposal_id, proposal);

        msg::reply(
            DaoEvent::SubmitFundingProposal {
                proposer: msg::source(),
                applicant: *applicant,
                proposal_id: self.proposal_id,
                amount,
            },
            0,
        );
        self.proposal_id = self.proposal_id.saturating_add(1);
    }

    /// The member (or the delegate address of the member) submit his vote (YES or NO) on the proposal
    /// Requirements:
    /// * The proposal can be submitted only by the existing members or their delegate addresses
    /// * The member can vote on the proposal only once
    /// * Proposal must exist, the voting period must has started and not expired
    /// Arguments:
    /// * `proposal_id`: the proposal ID
    /// * `vote`: the member  a member vote (YES or NO)
    fn submit_vote(&mut self, proposal_id: u128, vote: Vote) {
        self.check_for_membership();

        // checks that proposal exists, the voting period has started, not expired and that member did not vote on the proposal
        let proposal = match self.proposals.get_mut(&proposal_id) {
            Some(proposal) => {
                if exec::block_timestamp() > proposal.starting_period + self.voting_period_length {
                    panic!("proposal voting period has expired");
                }
                if exec::block_timestamp() < proposal.starting_period {
                    panic!("voting period has not started");
                }
                if proposal.votes_by_member.contains_key(&msg::source()) {
                    panic!("account has already voted on that proposal");
                }
                proposal
            }
            None => {
                panic!("proposal does not exist");
            }
        };

        let member = self.members.get_mut(&msg::source()).unwrap();

        match vote {
            Vote::Yes => {
                proposal.yes_votes = proposal.yes_votes.saturating_add(member.shares);
                // it is necessary to save the highest id of the proposal - must be processed for member to ragequit
                if member.highest_index_yes_vote < proposal_id {
                    member.highest_index_yes_vote = proposal_id;
                }
            }
            Vote::No => {
                proposal.no_votes = proposal.no_votes.saturating_add(member.shares);
            }
        }
        proposal.votes_by_member.insert(msg::source(), vote.clone());

        msg::reply(
            DaoEvent::SubmitVote {
                account: msg::source(),
                proposal_id,
                vote,
            },
            0,
        );
    }

    /// The proposal processing after the proposal completes during the grace period.
    /// If the proposal is accepted, the tribute tokens are deposited into the contract and new shares are minted and issued to the applicant.
    /// If the proposal is rejected, the tribute tokens are returned to the applicant.
    /// Requirements:
    /// * The previous proposal must be processed
    /// * The proposal must exist, be ready for processing
    /// * The proposal must not be cancelled, aborted or already be processed
    /// Arguments:
    /// * `proposal_id`: the proposal ID
    async fn process_proposal(&mut self, proposal_id: u128) {
        if proposal_id > 0 && !self.proposals.get(&(&proposal_id - 1)).unwrap().processed {
            panic!("Previous proposal must be processed");
        }
        let proposal = match self.proposals.get_mut(&proposal_id) {
            Some(proposal) => {
                if proposal.processed {
                    panic!("Proposal has already been processed");
                }
                if exec::block_timestamp()
                    < proposal.starting_period
                        + self.voting_period_length
                        + self.grace_period_length
                {
                    panic!("Proposal is not ready to be processed");
                }
                proposal
            }
            None => {
                panic!("proposal does not exist");
            }
        };

        proposal.processed = true;
        proposal.did_pass = proposal.yes_votes > proposal.no_votes
            && proposal.yes_votes * 10000 / self.total_shares >= proposal.quorum;

        // if funding propoposal has passed
        if proposal.did_pass  {
            transfer_tokens(
                &self.approved_token_program_id,
                &exec::program_id(),
                &proposal.applicant,
                proposal.amount,
            )
            .await;
        }
        msg::reply(
            DaoEvent::ProcessProposal {
                applicant: proposal.applicant,
                proposal_id,
                did_pass: proposal.did_pass,
            },
            0,
        );
    }


    // calculates the funds that the member can redeem based on his shares
    // async fn redeemable_funds(&self, share: u128) -> u128 {
    //     let balance = balance(&self.approved_token_program_id, &exec::program_id()).await;
    //     (share * balance) / self.total_shares
    // }

    // checks that account is DAO member
    fn is_member(&self, account: &ActorId) -> bool {
        match self.members.get(account) {
            Some(member) => {
                if member.shares == 0 {
                    return false;
                }
            }
            None => {
                return false;
            }
        }
        true
    }

    // check that `msg::source()` is either a DAO member or a delegate key
    fn check_for_membership(&self) {
        if !self.is_member(&msg::source()) {
            panic!("account is not a DAO member")
        } 
    }
}

gstd::metadata! {
    title: "DAO",
    init:
        input : InitDao,
    handle:
        input : DaoAction,
        output : DaoEvent,
    state:
        input: State,
        output: StateReply,
}

#[no_mangle]
pub unsafe extern "C" fn init() {
    let config: InitDao = msg::load().expect("Unable to decode InitDao");
    let mut dao = Dao {
        admin: config.admin,
        approved_token_program_id: config.approved_token_program_id,
        voting_period_length: config.voting_period_length,
        period_duration: config.period_duration,
        total_shares: 1,
        ..Dao::default()
    };
    dao.members.insert(
        config.admin,
        Member {
            shares: 1,
            highest_index_yes_vote: 0,
        },
    );
    DAO = Some(dao);
}

#[gstd::async_main]
async unsafe fn main() {
    let action: DaoAction = msg::load().expect("Could not load Action");
    let dao: &mut Dao = unsafe { DAO.get_or_insert(Dao::default()) };
    match action {
        DaoAction::Deposit{ amount } => dao.deposit(amount).await, 
        DaoAction::SubmitFundingProposal {
            applicant,
            amount,
            quorum,
            details,
        } => {
            dao.submit_funding_proposal(&applicant, amount, quorum, details)
                .await;
        }
        DaoAction::ProcessProposal { proposal_id } => {
            dao.process_proposal(proposal_id).await;
        }
        DaoAction::SubmitVote { proposal_id, vote } => {
            dao.submit_vote(proposal_id, vote);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn meta_state() -> *mut [i32; 2] {
    let state: State = msg::load().expect("failed to decode input argument");
    let dao: &mut Dao = DAO.get_or_insert(Dao::default());
    let encoded = match state {
        State::UserStatus(account) => {
            let role = if account == dao.admin {
                Role::Admin
            } else if dao.is_member(&account) {
                Role::Member
            } else {
                Role::None
            };
            StateReply::UserStatus(role).encode()
        }
        State::AllProposals => StateReply::AllProposals(dao.proposals.clone()).encode(),
        State::IsMember(account) => StateReply::IsMember(dao.is_member(&account)).encode(),
        State::ProposalId => StateReply::ProposalId(dao.proposal_id).encode(),
        State::ProposalInfo(proposal_id) => {
            StateReply::ProposalInfo(dao.proposals.get(&proposal_id).unwrap().clone()).encode()
        }
        State::MemberInfo(account) => {
            StateReply::MemberInfo(dao.members.get(&account).unwrap().clone()).encode()
        }
        State::MemberPower(account) => {
            let member = dao.members.get(&account).expect("Member does not exist");
            StateReply::MemberPower(member.shares).encode()
        }
    };
    let result = gstd::macros::util::to_wasm_ptr(&(encoded[..]));
    core::mem::forget(encoded);
    result
}
