import React from 'react';
import { View, Text, Button, Switch, TextInput } from 'react-native';

export default function AccountScreen() {
  const [isDark, setIsDark] = React.useState(false);
  const [name, setName] = React.useState('');
  return (
    <View style={{padding:20}}>
      <Text style={{fontSize:24, marginBottom:12}}>Account</Text>
      <TextInput placeholder="Full name" value={name} onChangeText={setName} style={{borderWidth:1,padding:8,marginBottom:12}} />
      <View style={{flexDirection:'row', alignItems:'center', marginBottom:12}}>
        <Text>Dark theme</Text>
        <Switch value={isDark} onValueChange={setIsDark} />
      </View>
      <Button title="Set Wallet (placeholder)" onPress={() => alert('Implement wallet connect')} />
      <View style={{height:12}} />
      <Button title="Support / Help" onPress={() => alert('Open support page')} />
    </View>
  );
}
