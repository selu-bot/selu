1. Apple Developer Setup (you need to do this in your Apple Developer account)

You need an APNs Auth Key (.p8 file). If you don't have one already:
- Go to https://developer.apple.com/account/resources/authkeys/list
- Click "+" to create a new key
- Enable "Apple Push Notifications service (APNs)"
- Download the .p8 file (you can only download it once!)
- Note the Key ID (10 characters, shown on the key page)
- Note your Team ID (shown in Membership details)

2. Orchestrator Configuration

Add these environment variables to your selu-core instance:

SELU__PUSH_RELAY_URL=https://selu.bot/api/relay/push
SELU__INSTANCE_ID=<your-instance-url, e.g. https://selu.example.com>

3. selu-site Lambda Environment Variables

These need to be added to the Lambda function (via CDK or manually):

APNS_KEY_PEM=<contents of your .p8 file>
APNS_KEY_ID=<10-char key ID>
APNS_TEAM_ID=<your team ID>
APNS_BUNDLE_ID=bot.selu.ios
APNS_ENVIRONMENT=production

Do you have the APNs .p8 key already, or do you need to create one? And would you like me to wire the APNs env vars into the CDK stack so
they're set during deployment?